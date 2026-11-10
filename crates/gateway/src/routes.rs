//! REST endpoints under `/v1` (SPEC §6; every endpoint is in `docs/api/openapi.yaml`).
//!
//! Commands go to the core and are answered only once durable. Reads of live state (markets,
//! books, portfolio) come from the engine between batches, so they never show an unacknowledged
//! command. History (fills, order events) comes from the PostgreSQL read models and says how
//! far they reach (`as_of`).
//!
//! Handlers do not repeat `# Errors` sections: their errors are the documented API error
//! responses (`docs/api/openapi.yaml`), produced by [`ApiError`].
#![allow(clippy::missing_errors_doc)]

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{FromRequest, Path, Query as UrlQuery, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use brier_core::{Answer, Committed, Query};
use brier_engine::{Command, Event, NewOrder, RejectReason};
use brier_types::{AccountId, MarketId, OrderId};
use brier_wire::{
    cancel_name, reject_name, side_from_name, side_name, state_from_name, state_name, tif_from_name,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::audit::{self, Actor};
use crate::auth::{self, Role, User};
use crate::error::ApiError;
use crate::totp;
use crate::{AppState, ClientIp};

pub(crate) type Shared = State<Arc<AppState>>;
pub(crate) type ApiResult = Result<Response, ApiError>;

/// Rows per page of history when the client does not say.
const DEFAULT_PAGE: i64 = 100;
/// Most rows per page of history.
const MAX_PAGE: i64 = 1_000;

/// Everything a handler needs from the request: authentication signs over the method, the
/// path with its query, and the exact body bytes.
pub struct Req {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    pub(crate) ip: IpAddr,
    /// The client address to record as a login signal, if the gateway knows it
    /// ([`crate::LoginAddress`]).
    pub(crate) signal: Option<IpAddr>,
    body: Bytes,
}

impl<S: Send + Sync> FromRequest<S> for Req {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        let (method, uri, headers) = (
            req.method().clone(),
            req.uri().clone(),
            req.headers().clone(),
        );
        let ip = req
            .extensions()
            .get::<ClientIp>()
            .map_or(IpAddr::from([0, 0, 0, 0]), |c| c.0);
        let signal = req
            .extensions()
            .get::<crate::LoginAddress>()
            .and_then(|a| a.0);
        let body = Bytes::from_request(req, state).await.map_err(|e| {
            if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "too_large",
                    "the request body is too large",
                )
            } else {
                ApiError::bad_request(e.body_text())
            }
        })?;
        Ok(Self {
            method,
            uri,
            headers,
            ip,
            signal,
            body,
        })
    }
}

impl Req {
    pub(crate) async fn user(&self, s: &AppState, allow_signed: bool) -> Result<User, ApiError> {
        let path = self
            .uri
            .path_and_query()
            .map_or_else(|| self.uri.path(), |p| p.as_str());
        auth::authenticate(
            &s.db,
            &s.replays,
            &self.headers,
            &self.method,
            path,
            &self.body,
            allow_signed,
        )
        .await
    }

    pub(crate) fn json<T: DeserializeOwned>(&self) -> Result<T, ApiError> {
        serde_json::from_slice(&self.body)
            .map_err(|e| ApiError::bad_request(format!("invalid body: {e}")))
    }

    pub(crate) fn query<T: DeserializeOwned>(&self) -> Result<T, ApiError> {
        UrlQuery::try_from_uri(&self.uri)
            .map(|q| q.0)
            .map_err(|e| ApiError::bad_request(e.body_text()))
    }
}

pub(crate) fn parse_id<T: std::str::FromStr>(raw: &str, what: &str) -> Result<T, ApiError> {
    raw.parse()
        .map_err(|_| ApiError::bad_request(format!("{what} must be a non-negative integer")))
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(&e))
}

// ---- identity -------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    username: String,
    password: String,
    /// The user confirms they are 18 or older.
    adult: bool,
    /// The version of the terms the user accepts ([`crate::account::TERMS_VERSION`]).
    terms: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginBody {
    username: String,
    password: String,
    /// The current TOTP code, required once TOTP is on.
    #[serde(default)]
    totp: Option<String>,
}

fn check_username(name: &str) -> Result<(), ApiError> {
    let ok = (3..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "usernames are 3 to 32 characters: a-z, 0-9, _",
        ))
    }
}

/// Make sure the user's engine account exists (registration opens it; this repairs a
/// registration that failed between the user row and the engine).
async fn ensure_account(s: &AppState, user: User) -> Result<(), ApiError> {
    let account = user.account()?;
    if let Answer::Account { view: Some(_), .. } = s.core.query(Query::Account(account)).await? {
        return Ok(());
    }
    let committed = s.core.submit(Command::OpenAccount { account }).await?;
    match committed.events.as_slice() {
        [Event::AccountOpened { .. }, ..]
        | [
            Event::Rejected {
                reason: RejectReason::AccountExists,
            },
        ] => Ok(()),
        _ => Err(ApiError::internal(&format!(
            "open_account answered {:?}",
            committed.events
        ))),
    }
}

/// `POST /v1/register`: create a user and its engine account.
pub async fn register(State(s): Shared, req: Req) -> ApiResult {
    if !s.auth_limits.allow(req.ip) {
        return Err(ApiError::rate_limited());
    }
    let Registration {
        username,
        password,
        adult,
        terms,
    } = req.json()?;
    check_username(&username)?;
    let address = match (s.totp_key, req.signal) {
        (Some(key), Some(ip)) => Some(crate::surveillance::address_hash(&key, ip)?),
        _ => None,
    };
    crate::account::check_signup(&s.db, adult, &terms, address.as_ref()).await?;
    let hash = blocking(move || auth::hash_password(&password)).await??;
    let row = s
        .db
        .query_opt("INSERT INTO users (username, password_hash) VALUES ($1, $2) ON CONFLICT (username) DO NOTHING RETURNING id", &[&username, &hash])
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "username_taken", "that username is taken"))?;
    let user = User {
        id: row.get(0),
        role: Role::Trader,
        mfa: false,
    };
    if let Err(e) = ensure_account(&s, user).await {
        // No half-made user: without its engine account the row is removed. If this delete
        // also fails, the next login opens the account instead.
        s.db.execute("DELETE FROM users WHERE id = $1", &[&user.id])
            .await?;
        return Err(e);
    }
    crate::account::record_signup(&s, user.id, address).await?;
    let codes = crate::account::new_codes(&s.db, user.id).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "user_id": user.id, "username": username, "recovery_codes": codes })),
    )
        .into_response())
}

/// `POST /v1/login`: exchange a username and password for a session token.
pub async fn login(State(s): Shared, req: Req) -> ApiResult {
    if !s.auth_limits.allow(req.ip) {
        return Err(ApiError::rate_limited());
    }
    let LoginBody {
        username,
        password,
        totp,
    } = req.json()?;
    let row =
        s.db.query_opt(
            "SELECT id, password_hash, role, totp_enabled, totp_secret FROM users WHERE username = $1",
            &[&username],
        )
        .await?;
    // An unknown username still costs one hash, so response time does not reveal which
    // usernames exist.
    let stored = row
        .as_ref()
        .map_or_else(|| s.dummy_hash.clone(), |r| r.get::<_, String>(1));
    let matches = blocking(move || auth::verify_password(&password, &stored)).await?;
    let (user, secret) = match row {
        Some(r) if matches => {
            let secret: Option<Vec<u8>> = if r.get::<_, bool>(3) { r.get(4) } else { None };
            let user = User {
                id: r.get(0),
                role: Role::from_name(r.get(2)),
                mfa: secret.is_some(),
            };
            (user, secret)
        }
        _ => return Err(ApiError::unauthorized("wrong username or password")),
    };
    if let Some(stored) = secret {
        let code = totp.ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "totp_required",
                "this account needs its TOTP code",
            )
        })?;
        check_totp(&s, user.id, &stored, &code).await?;
    }
    ensure_account(&s, user).await?;
    crate::surveillance::record_login(&s, user.id, req.signal).await?;
    let token = auth::new_session(&s.db, user.id, user.mfa).await?;
    let expires = i64::from(auth::SESSION_HOURS).saturating_mul(3_600);
    Ok(
        Json(json!({ "token": token, "user_id": user.id, "expires_in_seconds": expires }))
            .into_response(),
    )
}

/// Accept `code` for this user once: it must match a step within one of now, and be later than
/// the last step accepted.
pub(crate) async fn check_totp(
    s: &AppState,
    user: i64,
    stored: &[u8],
    code: &str,
) -> Result<(), ApiError> {
    let key = totp_key(s)?;
    let secret = totp::open_secret(&key, stored)?;
    let now_step = totp::step(auth::now_ms() / 1_000);
    let matched = totp::matching_step(&secret, code.trim(), now_step)
        .ok_or_else(|| ApiError::unauthorized("wrong TOTP code"))?;
    let step = i64::try_from(matched).map_err(|e| ApiError::internal(&e))?;
    let fresh = s
        .db
        .execute(
            "UPDATE users SET totp_last_step = $2 WHERE id = $1 AND (totp_last_step IS NULL OR totp_last_step < $2)",
            &[&user, &step],
        )
        .await?;
    if fresh == 1 {
        Ok(())
    } else {
        Err(ApiError::unauthorized("this TOTP code was already used"))
    }
}

fn totp_key(s: &AppState) -> Result<[u8; 32], ApiError> {
    s.totp_key.ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "TOTP is not configured on this exchange (BRIER_TOTP_KEY)",
        )
    })
}

/// `POST /v1/totp/setup`: a new TOTP secret for the caller (session only), not yet on.
pub async fn totp_setup(State(s): Shared, req: Req) -> ApiResult {
    let user = req.user(&s, false).await?;
    let key = totp_key(&s)?;
    let (secret, stored) = totp::new_secret(&key)?;
    let row = s
        .db
        .query_opt(
            "UPDATE users SET totp_secret = $2, totp_last_step = NULL WHERE id = $1 AND NOT totp_enabled RETURNING username",
            &[&user.id, &stored],
        )
        .await?
        .ok_or_else(|| ApiError::conflict("TOTP is already on for this account"))?;
    let username: String = row.get(0);
    let encoded = totp::base32(&secret);
    let uri = format!(
        "otpauth://totp/Brier:{username}?secret={encoded}&issuer=Brier&algorithm=SHA1&digits={}&period={}",
        totp::DIGITS,
        totp::STEP_SECONDS
    );
    Ok(Json(json!({ "secret": encoded, "otpauth_uri": uri })).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodeBody {
    code: String,
}

/// `POST /v1/totp/enable`: turn TOTP on with one valid code (session only). This session then
/// counts as signed in with TOTP.
pub async fn totp_enable(State(s): Shared, req: Req) -> ApiResult {
    let user = req.user(&s, false).await?;
    let CodeBody { code } = req.json()?;
    let row =
        s.db.query_one(
            "SELECT totp_secret, totp_enabled FROM users WHERE id = $1",
            &[&user.id],
        )
        .await?;
    if row.get::<_, bool>(1) {
        return Err(ApiError::conflict("TOTP is already on for this account"));
    }
    let stored: Vec<u8> = row
        .get::<_, Option<Vec<u8>>>(0)
        .ok_or_else(|| ApiError::conflict("set up TOTP first (POST /v1/totp/setup)"))?;
    check_totp(&s, user.id, &stored, &code).await?;
    s.db.execute(
        "UPDATE users SET totp_enabled = true WHERE id = $1",
        &[&user.id],
    )
    .await?;
    if let Some(token) = auth::bearer(&req.headers) {
        auth::mark_session_mfa(&s.db, token).await?;
    }
    audit::record(
        &s,
        Actor::User(user.id),
        "enable_totp",
        &json!({}),
        "user turned TOTP on",
    )
    .await?;
    Ok(Json(json!({ "totp": "on" })).into_response())
}

/// `GET /v1/me`: who the caller is: username, role, and whether TOTP is on and was used.
pub async fn me(State(s): Shared, req: Req) -> ApiResult {
    let user = req.user(&s, true).await?;
    let row =
        s.db.query_one(
            "SELECT username, role, totp_enabled FROM users WHERE id = $1",
            &[&user.id],
        )
        .await?;
    Ok(Json(json!({
        "user_id": user.id, "username": row.get::<_, &str>(0), "role": row.get::<_, &str>(1),
        "totp": row.get::<_, bool>(2), "signed_in_with_totp": user.mfa,
    }))
    .into_response())
}

/// `POST /v1/logout`: end the session.
pub async fn logout(State(s): Shared, req: Req) -> ApiResult {
    req.user(&s, false).await?;
    if let Some(token) = auth::bearer(&req.headers) {
        auth::end_session(&s.db, token).await?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewKey {
    public_key: String,
}

/// `POST /v1/keys`: register an Ed25519 public key for signed requests (session only).
pub async fn add_key(State(s): Shared, req: Req) -> ApiResult {
    let user = req.user(&s, false).await?;
    let NewKey { public_key } = req.json()?;
    let id = auth::add_key(&s.db, user.id, &public_key).await?;
    Ok((StatusCode::CREATED, Json(json!({ "key_id": id }))).into_response())
}

/// `GET /v1/keys`: the user's API keys (session only).
pub async fn list_keys(State(s): Shared, req: Req) -> ApiResult {
    let user = req.user(&s, false).await?;
    let rows = s
        .db
        .query(
            r#"SELECT id, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'), revoked_at IS NOT NULL FROM api_keys WHERE user_id = $1 ORDER BY id"#,
            &[&user.id],
        )
        .await?;
    let keys: Vec<Value> = rows
        .iter()
        .map(|r| json!({ "key_id": r.get::<_, i64>(0), "created_at": r.get::<_, String>(1), "revoked": r.get::<_, bool>(2) }))
        .collect();
    Ok(Json(json!({ "keys": keys })).into_response())
}

/// `DELETE /v1/keys/{id}`: revoke an API key (session only).
pub async fn revoke_key(State(s): Shared, Path(raw): Path<String>, req: Req) -> ApiResult {
    let user = req.user(&s, false).await?;
    let id: i64 = parse_id(&raw, "key id")?;
    let n = s
        .db
        .execute("UPDATE api_keys SET revoked_at = now() WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL", &[&id, &user.id])
        .await?;
    if n == 0 {
        return Err(ApiError::not_found("no such active key"));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---- market data ----------------------------------------------------------------------------

/// `GET /v1/health`: how far the journal and the read models reach.
pub async fn health(State(s): Shared) -> Json<Value> {
    let durable = *s.core.durable().borrow();
    Json(json!({ "durable_seq": durable, "projected_seq": s.projected.load(Ordering::Acquire) }))
}

/// A market's listing details for the market list: title, category, close time, and (from the
/// read models) the last trade price and the contracts traded.
async fn listing_details(s: &AppState) -> Result<std::collections::HashMap<i64, Value>, ApiError> {
    let rows = s
        .db
        .query(
            "SELECT b.market, b.canonical::jsonb ->> 'title', b.canonical::jsonb -> 'source' ->> 'provider', \
                    b.canonical::jsonb -> 'source' ->> 'series', extract(epoch FROM b.close_at)::bigint, \
                    (SELECT f.price FROM fills f WHERE f.market = b.market ORDER BY f.seq DESC, f.ordinal DESC LIMIT 1), \
                    (SELECT coalesce(sum(f.qty), 0)::bigint FROM fills f WHERE f.market = b.market) \
             FROM rulebooks b",
            &[],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let provider: Option<brier_rules::Provider> = r
                .get::<_, Option<&str>>(2)
                .and_then(|p| serde_json::from_value(Value::String(p.to_owned())).ok());
            let category = provider
                .zip(r.get::<_, Option<&str>>(3))
                .and_then(|(p, id)| brier_rules::series(p, id))
                .map(|series| series.category);
            let close: i64 = r.get(4);
            (
                r.get::<_, i64>(0),
                json!({
                    "title": r.get::<_, Option<&str>>(1), "category": category,
                    "close": crate::markets::iso(close),
                    "last_price": r.get::<_, Option<i16>>(5), "volume": r.get::<_, i64>(6),
                }),
            )
        })
        .collect())
}

/// `GET /v1/markets`: every market and its state, with its listing details.
pub async fn markets(State(s): Shared) -> ApiResult {
    let Answer::Markets { as_of, markets } = s.core.query(Query::Markets).await? else {
        return Err(ApiError::internal(&"markets query answered another shape"));
    };
    let details = listing_details(&s).await?;
    let items: Vec<Value> = markets
        .iter()
        .map(|&(m, st)| {
            let mut item = json!({ "market": m.get(), "state": state_name(st) });
            if let (Some(extra), Some(obj)) = (
                details.get(&i64::from(m.get())).and_then(Value::as_object),
                item.as_object_mut(),
            ) {
                obj.extend(extra.clone());
            }
            item
        })
        .collect();
    let projected = s.projected.load(Ordering::Acquire);
    Ok(
        Json(json!({ "as_of": as_of, "projected_seq": projected, "markets": items }))
            .into_response(),
    )
}

/// `GET /v1/markets/{id}/book`: aggregated price levels, best first.
pub async fn book(State(s): Shared, Path(raw): Path<String>) -> ApiResult {
    let market = MarketId::new(parse_id(&raw, "market")?);
    let Answer::Book {
        as_of,
        state,
        bids,
        asks,
        ..
    } = s.core.query(Query::Book(market)).await?
    else {
        return Err(ApiError::internal(&"book query answered another shape"));
    };
    let state = state.ok_or_else(|| ApiError::not_found("no such market"))?;
    let levels = |v: &[(brier_types::Price, u64)]| {
        v.iter()
            .map(|&(p, q)| json!([p.ticks(), q]))
            .collect::<Vec<_>>()
    };
    Ok(Json(json!({ "as_of": as_of, "market": market.get(), "state": state_name(state), "bids": levels(&bids), "asks": levels(&asks) }))
        .into_response())
}

// ---- orders ---------------------------------------------------------------------------------

/// An order as clients send it (REST body, or a WebSocket `place` message).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlaceBody {
    market: u32,
    side: String,
    price: u8,
    qty: u32,
    #[serde(default)]
    tif: Option<String>,
    #[serde(default)]
    post_only: bool,
    #[serde(default)]
    client_order_id: Option<u64>,
}

impl PlaceBody {
    /// The engine command for this order from `account`.
    pub(crate) fn command(self, account: AccountId) -> Result<Command, ApiError> {
        let side = side_from_name(&self.side)
            .ok_or_else(|| ApiError::bad_request("side must be buy or sell"))?;
        let tif = tif_from_name(self.tif.as_deref().unwrap_or("gtc"))
            .ok_or_else(|| ApiError::bad_request("tif must be gtc, ioc, or fok"))?;
        Ok(Command::Place(NewOrder {
            account,
            market: MarketId::new(self.market),
            side,
            price: self.price,
            qty: self.qty,
            tif,
            post_only: self.post_only,
            client_order_id: self.client_order_id,
        }))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AmendBody {
    qty: u32,
}

/// The engine command for a REST order body from `account`.
pub fn decode_order(body: &[u8], account: AccountId) -> Result<Command, ApiError> {
    serde_json::from_slice::<PlaceBody>(body)
        .map_err(|e| ApiError::bad_request(format!("invalid body: {e}")))?
        .command(account)
}

async fn trader(s: &AppState, req: &Req) -> Result<AccountId, ApiError> {
    let user = req.user(s, true).await?;
    if !user.role.trades() {
        return Err(ApiError::forbidden());
    }
    if !s.order_limits.allow(user.id) {
        return Err(ApiError::rate_limited());
    }
    user.account()
}

/// The client's view of a place: its order, fills as taker, and where the rest went. Other
/// accounts' identities and ledger entries are not exposed.
pub(crate) fn place_result(c: &Committed) -> Result<Value, ApiError> {
    let seq = c.seq.get();
    let mut order: Option<OrderId> = None;
    let (mut status, mut remaining, mut cancel_reason) = ("filled", 0, None);
    let (mut fills, mut filled) = (Vec::new(), 0u64);
    for e in &c.events {
        match *e {
            Event::Rejected { reason } => return Err(ApiError::rejected(reject_name(reason), seq)),
            Event::Duplicate { original } => {
                return Ok(json!({ "seq": seq, "status": "duplicate", "order": original.get() }));
            }
            Event::Accepted { order: o } => order = Some(o),
            Event::Trade(t) if Some(t.taker) == order => {
                fills.push(json!({ "price": t.price.ticks(), "qty": t.qty.contracts() }));
                filled = filled.saturating_add(u64::from(t.qty.contracts()));
            }
            Event::Rested {
                order: o,
                remaining: r,
            } if Some(o) == order => (status, remaining) = ("resting", r.contracts()),
            Event::Cancelled {
                order: o,
                remaining: r,
                reason,
            } if Some(o) == order => {
                (status, remaining, cancel_reason) =
                    ("cancelled", r.contracts(), Some(cancel_name(reason)));
            }
            _ => {}
        }
    }
    let order =
        order.ok_or_else(|| ApiError::internal(&format!("place answered {:?}", c.events)))?;
    Ok(json!({
        "seq": seq, "order": order.get(), "status": status, "filled": filled, "remaining": remaining,
        "cancel_reason": cancel_reason, "fills": fills,
    }))
}

/// The client's view of a cancel.
pub(crate) fn cancel_result(c: &Committed, order: OrderId) -> Result<Value, ApiError> {
    match c.events.as_slice() {
        [Event::Cancelled { remaining, .. }, ..] => Ok(
            json!({ "seq": c.seq.get(), "order": order.get(), "status": "cancelled", "cancelled": remaining.contracts() }),
        ),
        [Event::Rejected { reason }] => Err(ApiError::rejected(reject_name(*reason), c.seq.get())),
        other => Err(ApiError::internal(&format!("cancel answered {other:?}"))),
    }
}

/// The client's view of an amend.
pub(crate) fn amend_result(c: &Committed, order: OrderId) -> Result<Value, ApiError> {
    match c.events.as_slice() {
        [Event::Amended { remaining, .. }, ..] => Ok(
            json!({ "seq": c.seq.get(), "order": order.get(), "status": "resting", "remaining": remaining.contracts() }),
        ),
        [Event::Rejected { reason }] => Err(ApiError::rejected(reject_name(*reason), c.seq.get())),
        other => Err(ApiError::internal(&format!("amend answered {other:?}"))),
    }
}

/// `POST /v1/orders`: place an order.
pub async fn place(State(s): Shared, req: Req) -> ApiResult {
    let account = trader(&s, &req).await?;
    let command = decode_order(&req.body, account)?;
    Ok(Json(place_result(&crate::leases::submit(&s, command).await?)?).into_response())
}

/// `DELETE /v1/orders/{id}`: cancel a resting order.
pub async fn cancel(State(s): Shared, Path(raw): Path<String>, req: Req) -> ApiResult {
    let account = trader(&s, &req).await?;
    let order = OrderId::new(parse_id(&raw, "order")?);
    let c = s.core.submit(Command::Cancel { account, order }).await?;
    Ok(Json(cancel_result(&c, order)?).into_response())
}

/// `PATCH /v1/orders/{id}`: reduce a resting order's quantity, keeping its time priority.
pub async fn amend(State(s): Shared, Path(raw): Path<String>, req: Req) -> ApiResult {
    let account = trader(&s, &req).await?;
    let order = OrderId::new(parse_id(&raw, "order")?);
    let AmendBody { qty } = req.json()?;
    let c = crate::leases::submit(
        &s,
        Command::Amend {
            account,
            order,
            qty,
        },
    )
    .await?;
    Ok(Json(amend_result(&c, order)?).into_response())
}

/// `GET /v1/markets/{id}/trades`: the market's most recent executions, newest first, from the
/// read models (no account identities).
pub async fn trades(State(s): Shared, Path(raw): Path<String>, req: Req) -> ApiResult {
    let market = i64::from(parse_id::<u32>(&raw, "market")?);
    let (_, _, limit) = req.query::<Page>()?.bounds()?;
    let as_of = s.projected.load(Ordering::Acquire);
    let rows = s
        .db
        .query(
            "SELECT seq, ordinal, price, qty, taker_side FROM fills WHERE market = $1 ORDER BY seq DESC, ordinal DESC LIMIT $2",
            &[&market, &limit],
        )
        .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({ "seq": r.get::<_, i64>(0), "ordinal": r.get::<_, i32>(1), "price": r.get::<_, i16>(2), "qty": r.get::<_, i32>(3), "taker_side": r.get::<_, &str>(4) })
        })
        .collect();
    Ok(Json(json!({ "as_of": as_of, "market": market, "trades": items })).into_response())
}

/// `GET /v1/portfolio`: cash, collateral, positions, and open orders, from the engine.
pub async fn portfolio(State(s): Shared, req: Req) -> ApiResult {
    let account = req.user(&s, true).await?.account()?;
    let Answer::Account {
        as_of,
        view,
        open_orders,
    } = s.core.query(Query::Account(account)).await?
    else {
        return Err(ApiError::internal(&"account query answered another shape"));
    };
    let view = view
        .ok_or_else(|| ApiError::not_found("this user has no engine account yet; log in once"))?;
    let available = view
        .cash
        .checked_sub(view.reserved)
        .map_err(|e| ApiError::internal(&e))?;
    let positions: Vec<Value> = view
        .positions
        .iter()
        .map(|&(m, p)| json!({ "market": m.get(), "position": p }))
        .collect();
    let orders: Vec<Value> = open_orders
        .iter()
        .map(|(m, side, o)| {
            json!({ "order": o.id.get(), "market": m.get(), "side": side_name(*side), "price": o.price.ticks(), "remaining": o.remaining.contracts() })
        })
        .collect();
    Ok(Json(json!({
        "as_of": as_of, "cash": view.cash.cents(), "reserved": view.reserved.cents(), "available": available.cents(),
        "positions": positions, "open_orders": orders,
    }))
    .into_response())
}

// ---- history (read models) ------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    after_seq: Option<i64>,
    after_ordinal: Option<i32>,
    limit: Option<i64>,
}

impl Page {
    fn bounds(&self) -> Result<(i64, i32, i64), ApiError> {
        let limit = self.limit.unwrap_or(DEFAULT_PAGE);
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err(ApiError::bad_request(format!(
                "limit must be 1 to {MAX_PAGE}"
            )));
        }
        Ok((
            self.after_seq.unwrap_or(0),
            self.after_ordinal.unwrap_or(-1),
            limit,
        ))
    }
}

/// The cursor for the page after `rows`, or null when this page was the last.
fn next_page(last: Option<(i64, i32)>, len: usize, limit: i64) -> Value {
    match last {
        Some((seq, ordinal)) if i64::try_from(len).is_ok_and(|n| n == limit) => {
            json!({ "after_seq": seq, "after_ordinal": ordinal })
        }
        _ => Value::Null,
    }
}

/// `GET /v1/fills`: the user's executions, oldest first, from the read models.
pub async fn fills(State(s): Shared, req: Req) -> ApiResult {
    let user = req.user(&s, true).await?;
    let (after_seq, after_ordinal, limit) = req.query::<Page>()?.bounds()?;
    let as_of = s.projected.load(Ordering::Acquire);
    let rows = s
        .db
        .query(
            "SELECT seq, ordinal, market, \
                    CASE WHEN taker_account = $1 THEN taker ELSE maker END, \
                    CASE WHEN taker_account = $1 THEN taker_side WHEN taker_side = 'buy' THEN 'sell' ELSE 'buy' END, \
                    price, qty, taker_account = $1 \
             FROM fills WHERE (maker_account = $1 OR taker_account = $1) AND (seq, ordinal) > ($2, $3) \
             ORDER BY seq, ordinal LIMIT $4",
            &[&user.id, &after_seq, &after_ordinal, &limit],
        )
        .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "seq": r.get::<_, i64>(0), "ordinal": r.get::<_, i32>(1), "market": r.get::<_, i64>(2),
                "order": r.get::<_, i64>(3), "side": r.get::<_, &str>(4), "price": r.get::<_, i16>(5),
                "qty": r.get::<_, i32>(6), "liquidity": if r.get::<_, bool>(7) { "taker" } else { "maker" },
            })
        })
        .collect();
    let last = rows
        .last()
        .map(|r| (r.get::<_, i64>(0), r.get::<_, i32>(1)));
    Ok(
        Json(json!({ "as_of": as_of, "fills": items, "next": next_page(last, rows.len(), limit) }))
            .into_response(),
    )
}

fn order_event_json(r: &tokio_postgres::Row) -> Value {
    json!({
        "seq": r.get::<_, i64>(0), "ordinal": r.get::<_, i32>(1), "order": r.get::<_, Option<i64>>(2),
        "market": r.get::<_, i64>(3), "kind": r.get::<_, &str>(4), "remaining": r.get::<_, Option<i32>>(5),
        "reason": r.get::<_, Option<&str>>(6),
    })
}

/// `GET /v1/orders/history`: the user's order events, oldest first, from the read models.
pub async fn history(State(s): Shared, req: Req) -> ApiResult {
    let user = req.user(&s, true).await?;
    let (after_seq, after_ordinal, limit) = req.query::<Page>()?.bounds()?;
    let as_of = s.projected.load(Ordering::Acquire);
    let rows =
        s.db.query(
            "SELECT seq, ordinal, order_id, market, kind, remaining, reason FROM order_events \
             WHERE account = $1 AND (seq, ordinal) > ($2, $3) ORDER BY seq, ordinal LIMIT $4",
            &[&user.id, &after_seq, &after_ordinal, &limit],
        )
        .await?;
    let items: Vec<Value> = rows.iter().map(order_event_json).collect();
    let last = rows
        .last()
        .map(|r| (r.get::<_, i64>(0), r.get::<_, i32>(1)));
    Ok(
        Json(
            json!({ "as_of": as_of, "events": items, "next": next_page(last, rows.len(), limit) }),
        )
        .into_response(),
    )
}

/// `GET /v1/orders/{id}`: one order's events, from the read models.
pub async fn order(State(s): Shared, Path(raw): Path<String>, req: Req) -> ApiResult {
    let user = req.user(&s, true).await?;
    let id: i64 = parse_id(&raw, "order")?;
    let as_of = s.projected.load(Ordering::Acquire);
    let rows =
        s.db.query(
            "SELECT seq, ordinal, order_id, market, kind, remaining, reason FROM order_events \
             WHERE order_id = $1 AND account = $2 ORDER BY seq, ordinal",
            &[&id, &user.id],
        )
        .await?;
    if rows.is_empty() {
        return Err(ApiError::not_found(format!(
            "no order {id} of yours in the read models (as of seq {as_of})"
        )));
    }
    let items: Vec<Value> = rows.iter().map(order_event_json).collect();
    Ok(Json(json!({ "as_of": as_of, "order": id, "events": items })).into_response())
}

// ---- administration -------------------------------------------------------------------------

/// The signed-in user, who must be an admin.
pub(crate) async fn admin_user(s: &AppState, req: &Req) -> Result<User, ApiError> {
    let user = req.user(s, true).await?;
    if user.role != Role::Admin {
        return Err(ApiError::forbidden());
    }
    if !user.mfa {
        return Err(ApiError::forbidden_because(
            "admin actions need a sign-in with TOTP: turn TOTP on, then sign in with a code",
        ));
    }
    Ok(user)
}

/// Run an administrative command: admin role required, a reason required, and the command
/// audited ([`audit::submit`]).
pub(crate) async fn admin(
    s: &AppState,
    req: &Req,
    action: &'static str,
    command: Command,
    detail: Value,
    reason: &str,
) -> ApiResult {
    let user = admin_user(s, req).await?;
    let reason = audit::reason(reason)?;
    let done = audit::submit(s, Actor::User(user.id), action, command, detail, reason).await?;
    let seq = done.committed.seq.get();
    if let [Event::Rejected { reason }] = done.committed.events.as_slice() {
        return Err(ApiError::rejected(reject_name(*reason), seq));
    }
    Ok(
        Json(json!({ "seq": seq, "audit_id": done.audit_id, "events": done.events }))
            .into_response(),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateBody {
    state: String,
    reason: String,
}

/// `POST /v1/admin/markets/{id}/state`: move a market through its lifecycle.
pub async fn set_state(State(s): Shared, Path(raw): Path<String>, req: Req) -> ApiResult {
    let market: u32 = parse_id(&raw, "market")?;
    let b: StateBody = req.json()?;
    let state =
        state_from_name(&b.state).ok_or_else(|| ApiError::bad_request("unknown market state"))?;
    crate::markets::check_manual_state(&s, MarketId::new(market), state).await?;
    let command = Command::SetState {
        market: MarketId::new(market),
        state,
    };
    admin(
        &s,
        &req,
        "set_state",
        command,
        json!({ "market": market, "state": b.state }),
        &b.reason,
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelAllBody {
    #[serde(default)]
    account: Option<u64>,
    #[serde(default)]
    market: Option<u32>,
    reason: String,
}

/// `POST /v1/admin/cancel-all`: kill switch; cancel resting orders by account and/or market.
pub async fn cancel_all(State(s): Shared, req: Req) -> ApiResult {
    let b: CancelAllBody = req.json()?;
    let command = Command::CancelAll {
        account: b.account.map(AccountId::new),
        market: b.market.map(MarketId::new),
    };
    admin(
        &s,
        &req,
        "cancel_all",
        command,
        json!({ "account": b.account, "market": b.market }),
        &b.reason,
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReasonBody {
    reason: String,
}

/// `POST /v1/admin/halt-all`: kill switch; halt every open market.
pub async fn halt_all(State(s): Shared, req: Req) -> ApiResult {
    let b: ReasonBody = req.json()?;
    admin(&s, &req, "halt_all", Command::HaltAll, json!({}), &b.reason).await
}

/// Any path that is not an endpoint.
pub async fn not_found() -> ApiError {
    ApiError::not_found("no such endpoint")
}
