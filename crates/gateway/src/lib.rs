//! The Brier gateway (SPEC §6): identity, signed requests, rate limits, REST, and the task that
//! keeps the PostgreSQL read models current.
//!
//! The gateway runs the core in-process ([`brier_core::start`]): one process owns the journal,
//! so there is exactly one writer. Every command a client sends is answered only after its
//! batch is durable.

pub mod account;
pub mod allowance;
pub mod audit;
pub mod auth;
pub mod clock;
pub mod error;
pub mod events;
pub mod feed;
pub mod forecasts;
pub mod leases;
pub mod limits;
pub mod markets;
pub mod resolution;
pub mod routes;
pub mod scheduler;
pub mod social;
pub mod surveillance;
pub mod totp;
pub mod ws;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::connect_info::ConnectInfo;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, on};
use brier_core::CoreHandle;
use brier_readmodel::{Projector, ReadModelError};
use serde_json::json;
use sha2::Digest as _;
use tokio::sync::watch;
use tokio_postgres::Client;

use crate::auth::ReplayCache;
use crate::clock::{Clock, Unix};
use crate::error::ApiError;
use crate::limits::Limiter;

/// Largest request body accepted. The largest legitimate body (an order) is under 300 bytes.
pub const MAX_BODY_BYTES: usize = 8 * 1_024;
/// Longest a request may take before the client is told to retry. An order that times out may
/// still have been accepted; a retry with the same `client_order_id` is safe (SPEC §6).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Most journal commands written to the read models in one transaction.
pub const PROJECT_BATCH: u64 = 10_000;
/// How long a confirmed resolution stays open to disputes before it settles (G5 plan): long
/// enough for a person to notice a wrong outcome. `BRIER_DISPUTE_WINDOW_SECONDS` overrides it.
pub const DISPUTE_WINDOW: Unix = 24 * 3_600;

/// The client address, as the gateway saw it, for per-address limits.
#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub IpAddr);

/// The client address to record as a login signal: the address a keyed proxy forwarded, or a
/// direct client's own address. `None` when only a proxy's address is known (recording it
/// would link every user behind the proxy), or for loopback and unspecified addresses.
#[derive(Debug, Clone, Copy)]
pub struct LoginAddress(pub Option<IpAddr>);

/// Header the web server sends to prove it is the configured proxy (`BRIER_PROXY_KEY`).
pub const PROXY_KEY_HEADER: &str = "x-brier-proxy-key";
/// Header carrying the client address the proxy saw, trusted only with a valid proxy key.
pub const CLIENT_ADDRESS_HEADER: &str = "x-brier-client-address";

/// Everything the handlers share.
pub struct AppState {
    /// The running core.
    pub core: CoreHandle,
    /// Serializes lease changes/cancellations against order submission. Ordinary orders share
    /// the read side, retaining concurrency while preventing a place-after-expiry race.
    pub order_safety: tokio::sync::RwLock<()>,
    /// Identity tables and read models. One connection: tokio-postgres pipelines concurrent
    /// queries over it.
    pub db: Client,
    /// Signatures seen inside the clock-skew window.
    pub replays: ReplayCache,
    /// Orders per account.
    pub order_limits: Limiter<i64>,
    /// Requests per client address.
    pub address_limits: Limiter<IpAddr>,
    /// Login and registration attempts per client address.
    pub auth_limits: Limiter<IpAddr>,
    /// The last sequence number reflected in the read models.
    pub projected: Arc<AtomicU64>,
    /// The market-data publisher.
    pub feed: feed::FeedHandle,
    /// Set to true to close every WebSocket connection and stop the scheduler (graceful
    /// shutdown).
    pub shutdown: watch::Sender<bool>,
    /// The clock schedules, proposals, and dispute windows are judged by.
    pub clock: Clock,
    /// Seconds a confirmed resolution stays open to disputes.
    pub dispute_window: Unix,
    /// The key TOTP secrets are encrypted under (`BRIER_TOTP_KEY`); without it, TOTP cannot be
    /// set up and admin actions are unavailable.
    pub totp_key: Option<[u8; 32]>,
    /// SHA-256 of the shared key that lets the web server forward client addresses
    /// (`BRIER_PROXY_KEY`); without it, forwarded addresses are never trusted.
    pub proxy_key: Option<[u8; 32]>,
    /// A hash checked when the username is unknown, so login time does not reveal it.
    dummy_hash: String,
}

impl AppState {
    /// State for a running core and a migrated database; starts the market-data publisher, so
    /// it must be called inside a Tokio runtime.
    ///
    /// # Errors
    /// [`ApiError`] if hashing the timing-equalizer password fails.
    pub fn new(core: CoreHandle, db: Client) -> Result<Self, ApiError> {
        Ok(Self {
            feed: feed::spawn(core.clone()),
            shutdown: watch::channel(false).0,
            core,
            order_safety: tokio::sync::RwLock::new(()),
            db,
            replays: ReplayCache::default(),
            order_limits: Limiter::per_second(
                limits::ACCOUNT_ORDERS_PER_SECOND,
                limits::ACCOUNT_ORDER_BURST,
            ),
            address_limits: Limiter::per_second(
                limits::ADDRESS_REQUESTS_PER_SECOND,
                limits::ADDRESS_REQUEST_BURST,
            ),
            auth_limits: Limiter::per_minute(
                limits::ADDRESS_AUTH_PER_MINUTE,
                limits::ADDRESS_AUTH_BURST,
            ),
            projected: Arc::new(AtomicU64::new(0)),
            clock: Clock::system(),
            dispute_window: DISPUTE_WINDOW,
            totp_key: None,
            proxy_key: None,
            dummy_hash: auth::hash_password("timing equalizer, never a real password")?,
        })
    }
}

impl AppState {
    /// The same state judged by another clock (tests use a manual one).
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The same state trusting client addresses forwarded with this proxy key.
    #[must_use]
    pub fn with_proxy_key(mut self, key: &str) -> Self {
        self.proxy_key = Some(sha2::Sha256::digest(key.as_bytes()).into());
        self
    }

    /// The same state with a TOTP key.
    #[must_use]
    pub const fn with_totp_key(mut self, key: [u8; 32]) -> Self {
        self.totp_key = Some(key);
        self
    }

    /// The same state with another dispute window, in seconds.
    #[must_use]
    pub const fn with_dispute_window(mut self, seconds: Unix) -> Self {
        self.dispute_window = seconds;
        self
    }
}

enum Forwarded {
    /// The configured proxy, with the client address it saw.
    Client(IpAddr),
    /// The configured proxy, without a client address.
    ProxyOnly,
    /// Anything else: forwarded headers, if any, are ignored.
    Direct,
}

/// Whether the request comes from the configured proxy, and the client address it forwarded.
/// Without `BRIER_PROXY_KEY`, or with a wrong key, forwarded headers are ignored: anyone could
/// send them.
fn forwarded(s: &AppState, headers: &axum::http::HeaderMap) -> Forwarded {
    let (Some(expected), Some(given)) = (s.proxy_key, headers.get(PROXY_KEY_HEADER)) else {
        return Forwarded::Direct;
    };
    let given: [u8; 32] = sha2::Sha256::digest(given.as_bytes()).into();
    // Compare in constant time: the key must not leak through response timing.
    if expected
        .iter()
        .zip(given)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        != 0
    {
        return Forwarded::Direct;
    }
    headers
        .get(CLIENT_ADDRESS_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<IpAddr>().ok())
        .map_or(Forwarded::ProxyOnly, |a| {
            Forwarded::Client(a.to_canonical())
        })
}

/// Per-address limit and request timeout, for every request.
async fn guard(State(s): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    let direct = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |c| {
            c.0.ip().to_canonical()
        });
    let (ip, signal) = match forwarded(&s, req.headers()) {
        Forwarded::Client(a) => (a, Some(a)),
        Forwarded::ProxyOnly => (direct, None),
        Forwarded::Direct => (
            direct,
            (!direct.is_loopback() && !direct.is_unspecified()).then_some(direct),
        ),
    };
    if !s.address_limits.allow(ip) {
        return ApiError::rate_limited().into_response();
    }
    req.extensions_mut().insert(ClientIp(ip));
    req.extensions_mut().insert(LoginAddress(signal));
    tokio::time::timeout(REQUEST_TIMEOUT, next.run(req))
        .await
        .unwrap_or_else(|_| {
            ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "timeout",
                "the request timed out; an order retried with the same client_order_id is safe",
            )
            .into_response()
        })
}

/// Declares every endpoint once: the router is built from this table, and [`ENDPOINTS`] lists
/// the same table so a test can hold `docs/api/openapi.yaml` to exactly what is served.
macro_rules! endpoints {
    ($($method:ident $path:literal => $handler:path),* $(,)?) => {
        /// Every endpoint the gateway serves, as (method, path).
        pub const ENDPOINTS: &[(&str, &str)] = &[$((stringify!($method), $path)),*];

        fn routes(router: Router<Arc<AppState>>) -> Router<Arc<AppState>> {
            router $(.route($path, on(MethodFilter::$method, $handler)))*
        }
    };
}

endpoints! {
    GET "/v1/health" => routes::health,
    POST "/v1/register" => routes::register,
    POST "/v1/login" => routes::login,
    POST "/v1/logout" => routes::logout,
    GET "/v1/me" => routes::me,
    DELETE "/v1/me" => account::delete,
    GET "/v1/me/export" => account::export,
    POST "/v1/recover" => account::recover,
    POST "/v1/recovery-codes" => account::regenerate,
    GET "/v1/allowance" => allowance::status,
    POST "/v1/allowance" => allowance::claim,
    POST "/v1/totp/setup" => routes::totp_setup,
    POST "/v1/totp/enable" => routes::totp_enable,
    POST "/v1/keys" => routes::add_key,
    GET "/v1/keys" => routes::list_keys,
    DELETE "/v1/keys/{id}" => routes::revoke_key,
    GET "/v1/markets" => routes::markets,
    GET "/v1/events" => events::list,
    GET "/v1/events/{slug}" => events::get,
    GET "/v1/markets/{id}/book" => routes::book,
    GET "/v1/markets/{id}/rules" => markets::rules,
    GET "/v1/markets/{id}/prices" => forecasts::prices,
    GET "/v1/markets/{id}/resolution" => resolution::get,
    POST "/v1/markets/{id}/resolution" => resolution::propose,
    POST "/v1/resolutions/{id}/disputes" => resolution::dispute,
    POST "/v1/resolutions/{id}/verify" => resolution::verify,
    PUT "/v1/markets/{id}/forecast" => forecasts::put,
    GET "/v1/forecasts" => forecasts::mine,
    GET "/v1/scorecard" => forecasts::scorecard,
    GET "/v1/leaderboards/skill" => forecasts::skill_leaderboard,
    GET "/v1/leaderboards/profit" => forecasts::profit_leaderboard,
    GET "/v1/markets/{id}/trades" => routes::trades,
    GET "/v1/markets/{id}/holders" => social::holders,
    GET "/v1/markets/{id}/activity" => social::activity,
    GET "/v1/users/{name}" => social::profile,
    GET "/v1/notifications" => social::notifications,
    POST "/v1/notifications/seen" => social::seen,
    GET "/v1/ws" => ws::upgrade,
    POST "/v1/orders" => routes::place,
    DELETE "/v1/orders" => leases::cancel_mine,
    POST "/v1/orders/cancel-after" => leases::heartbeat,
    GET "/v1/orders/history" => routes::history,
    GET "/v1/orders/{id}" => routes::order,
    DELETE "/v1/orders/{id}" => routes::cancel,
    PATCH "/v1/orders/{id}" => routes::amend,
    GET "/v1/portfolio" => routes::portfolio,
    GET "/v1/fills" => routes::fills,
    POST "/v1/admin/markets" => markets::create_market,
    POST "/v1/admin/markets/{id}/state" => routes::set_state,
    POST "/v1/admin/resolutions/{id}/confirm" => resolution::confirm,
    POST "/v1/admin/resolutions/{id}/withdraw" => resolution::withdraw,
    POST "/v1/admin/disputes/{id}/rule" => resolution::rule,
    GET "/v1/admin/queue" => resolution::queue,
    GET "/v1/admin/audit" => resolution::audit_log,
    GET "/v1/admin/cases" => surveillance::cases,
    GET "/v1/admin/cases/{id}" => surveillance::case,
    POST "/v1/admin/cases/{id}/close" => surveillance::close_case,
    GET "/v1/surveillance/links" => surveillance::links,
    POST "/v1/surveillance/cases" => surveillance::open_case,
    POST "/v1/admin/cancel-all" => routes::cancel_all,
    POST "/v1/admin/users/{id}/close" => account::close,
    POST "/v1/admin/halt-all" => routes::halt_all,
}

/// The `/v1` API: every endpoint in [`ENDPOINTS`], a JSON 404 for anything else, the body
/// limit, and the per-address limit and timeout on every request.
pub fn router(state: Arc<AppState>) -> Router {
    routes(Router::new())
        .fallback(routes::not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(Arc::clone(&state), guard))
        .with_state(state)
}

pub(crate) fn log(level: &str, msg: &str, detail: &dyn std::fmt::Display) {
    eprintln!(
        "{}",
        json!({ "level": level, "msg": msg, "detail": detail.to_string() })
    );
}

/// Keep the read models current: follow the journal in `data` up to the durable sequence
/// number, one transaction per [`PROJECT_BATCH`] commands. Returns `Ok` when the core stops; on
/// a failure it logs, waits, and resumes from the committed cursor, unless the database
/// connection itself is gone.
///
/// # Errors
/// The last [`ReadModelError`] once the database connection has closed.
pub async fn run_projector(
    mut db: Client,
    data: PathBuf,
    mut durable: watch::Receiver<u64>,
    projected: Arc<AtomicU64>,
) -> Result<(), ReadModelError> {
    loop {
        match follow(&mut db, &data, &mut durable, &projected).await {
            Ok(()) => return Ok(()),
            Err(e) if db.is_closed() => return Err(e),
            Err(e) => {
                log("error", "projector failed; resuming from its cursor", &e);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn follow(
    db: &mut Client,
    data: &std::path::Path,
    durable: &mut watch::Receiver<u64>,
    projected: &AtomicU64,
) -> Result<(), ReadModelError> {
    let mut projector = Projector::open(db, data).await?;
    projected.store(projector.cursor(), Ordering::Release);
    loop {
        let target = *durable.borrow_and_update();
        while projector.cursor() < target {
            if projector.project(db, target, PROJECT_BATCH).await? == 0 {
                // Durable but not yet visible to this reader; look again shortly.
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            projected.store(projector.cursor(), Ordering::Release);
        }
        if durable.changed().await.is_err() {
            return Ok(()); // the core stopped
        }
    }
}
