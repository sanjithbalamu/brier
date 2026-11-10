//! WebSocket `/v1/ws` (SPEC §6; every message is in `docs/api/asyncapi.yaml`): market data
//! subscriptions and order entry over one connection.
//!
//! Client messages are JSON objects with an `op`:
//! - `auth` with `token` (a session), or `key`, `timestamp`, `signature` (an Ed25519 signature
//!   of a `GET /v1/ws` request with an empty body, exactly as for REST);
//! - `subscribe` / `unsubscribe` with a `channel` (`book:<m>`, `trades:<m>`, `account`);
//! - `place` (the REST order body), `cancel` (`order`), `amend` (`order`, `qty`);
//! - `ping`.
//!
//! An optional `id` is echoed in the `reply`. Order replies arrive when the command is durable,
//! and may arrive in a different order from the requests; the `id` pairs them.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, HeaderValue, Method};
use axum::response::Response;
use brier_engine::Command;
use brier_types::{AccountId, OrderId};
use futures_util::SinkExt;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;

use crate::auth::{self, User};
use crate::error::ApiError;
use crate::feed::{Channel, FeedHandle, Subscription};
use crate::routes::{PlaceBody, amend_result, cancel_result, place_result};
use crate::{AppState, ClientIp};

/// Every client `op`, as `docs/api/asyncapi.yaml` lists them.
pub const OPS: &[&str] = &[
    "ping",
    "auth",
    "subscribe",
    "unsubscribe",
    "place",
    "cancel",
    "amend",
];
/// Every server message `type`, as `docs/api/asyncapi.yaml` lists them.
pub const SERVER_MESSAGES: &[&str] = &[
    "reply",
    "snapshot",
    "delta",
    "trade",
    "subscribed",
    "resync",
    "order",
    "fill",
];

/// Most queued messages written with one flush. Bounds how long the first waits for the rest.
const MAX_COALESCED: usize = 64;

/// Largest client message. The largest legitimate one (a `place`) is under 300 bytes.
pub const MAX_MESSAGE_BYTES: usize = 8 * 1_024;
/// Channels one connection may subscribe to at once.
pub const MAX_SUBSCRIPTIONS: usize = 64;
/// Orders one connection may have waiting for durability at once.
pub const MAX_IN_FLIGHT: usize = 256;
/// Messages queued for a connection's socket. A subscriber that falls this far behind stops
/// draining its channels and so lags them, which resubscribes it (a fresh book snapshot).
const OUT_QUEUE: usize = 1_024;

/// `GET /v1/ws`: upgrade to a WebSocket.
pub async fn upgrade(
    State(s): State<Arc<AppState>>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    ws: WebSocketUpgrade,
) -> Response {
    ws.max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| connection(s, socket, ip))
}

struct Session {
    user: Option<User>,
    subscriptions: HashMap<Channel, JoinHandle<()>>,
    in_flight: Arc<Semaphore>,
}

fn frame(v: &Value) -> Utf8Bytes {
    Utf8Bytes::from(v.to_string())
}

fn reply(id: Option<&Value>, result: Result<Value, ApiError>) -> Utf8Bytes {
    frame(&match result {
        Ok(v) => json!({ "type": "reply", "id": id, "ok": true, "result": v }),
        Err(e) => json!({ "type": "reply", "id": id, "ok": false, "error": e.body() }),
    })
}

async fn connection(s: Arc<AppState>, mut socket: WebSocket, ip: IpAddr) {
    let (out, mut outbox) = mpsc::channel::<Utf8Bytes>(OUT_QUEUE);
    let mut shutdown = s.shutdown.subscribe();
    let mut session = Session {
        user: None,
        subscriptions: HashMap::new(),
        in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
    };
    let mut closing = None;
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    let answer = if s.address_limits.allow(ip) {
                        handle(&s, &mut session, &out, text.as_str()).await
                    } else {
                        Some(reply(None, Err(ApiError::rate_limited())))
                    };
                    if let Some(answer) = answer
                        && socket.send(Message::Text(answer)).await.is_err()
                    {
                        break;
                    }
                }
                Some(Ok(Message::Binary(_))) => {
                    closing = Some((close_code::UNSUPPORTED, "text frames only"));
                    break;
                }
                Some(Ok(Message::Close(_))) => {
                    // The library queues the close reply (RFC 6455 §5.5.1); reading on sends it
                    // and then sees the connection end.
                    let drain = async { while let Some(Ok(_)) = socket.recv().await {} };
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), drain).await;
                    break;
                }
                Some(Err(_)) | None => break,
                Some(Ok(_)) => {} // ping and pong: answered by the library
            },
            Some(message) = outbox.recv() => {
                if write_queued(&mut socket, message, &mut outbox).await.is_err() {
                    break;
                }
            }
            _ = shutdown.changed() => {
                closing = Some((close_code::AWAY, "the exchange is shutting down"));
                break;
            }
        }
    }
    for (_, task) in session.subscriptions.drain() {
        task.abort();
    }
    if let Some((code, reason)) = closing {
        let _ = socket
            .send(Message::Close(Some(CloseFrame {
                code,
                reason: Utf8Bytes::from_static(reason),
            })))
            .await;
    }
}

/// Write `first` and whatever else is already queued, then flush once: one system call for
/// the lot instead of one per message (docs/notes/gateway.md, P4-coalesced).
async fn write_queued(
    socket: &mut WebSocket,
    first: Utf8Bytes,
    outbox: &mut mpsc::Receiver<Utf8Bytes>,
) -> Result<(), axum::Error> {
    socket.feed(Message::Text(first)).await?;
    for _ in 1..MAX_COALESCED {
        let Ok(next) = outbox.try_recv() else { break };
        socket.feed(Message::Text(next)).await?;
    }
    socket.flush().await
}

fn parse<T: DeserializeOwned>(body: Map<String, Value>) -> Result<T, ApiError> {
    serde_json::from_value(Value::Object(body))
        .map_err(|e| ApiError::bad_request(format!("invalid message: {e}")))
}

/// A client message: its `op`, its `id`, and the remaining fields.
pub type Parsed = (String, Option<Value>, Map<String, Value>);

/// A client message split into its `op`, its `id` (echoed in the reply), and the rest.
///
/// # Errors
/// The `id` (if one could be read) and a `bad_request` error for anything that is not a JSON
/// object with a string `op`.
pub fn parse_message(text: &str) -> Result<Parsed, (Option<Value>, ApiError)> {
    let mut body: Map<String, Value> = serde_json::from_str(text).map_err(|e| {
        (
            None,
            ApiError::bad_request(format!("messages are JSON objects: {e}")),
        )
    })?;
    let id = body.remove("id");
    match body.remove("op") {
        Some(Value::String(op)) => Ok((op, id, body)),
        _ => Err((id, ApiError::bad_request("every message needs an op"))),
    }
}

/// The engine command for a `place`, `cancel`, or `amend` message from `account`.
///
/// # Errors
/// `bad_request` for a malformed body or an op that is not an order op.
pub fn order_command(
    op: &str,
    body: Map<String, Value>,
    account: AccountId,
) -> Result<Command, ApiError> {
    match op {
        "place" => parse::<PlaceBody>(body)?.command(account),
        "cancel" => Ok(Command::Cancel {
            account,
            order: OrderId::new(parse::<OrderMessage>(body)?.order),
        }),
        "amend" => {
            let AmendMessage { order, qty } = parse(body)?;
            Ok(Command::Amend {
                account,
                order: OrderId::new(order),
                qty,
            })
        }
        _ => Err(ApiError::bad_request("not an order op")),
    }
}

/// Handle one client message; returns the immediate answer, if any (order replies come later,
/// through the outbox).
async fn handle(
    s: &Arc<AppState>,
    session: &mut Session,
    out: &mpsc::Sender<Utf8Bytes>,
    text: &str,
) -> Option<Utf8Bytes> {
    let (op, id, body) = match parse_message(text) {
        Ok(parts) => parts,
        Err((id, e)) => return Some(reply(id.as_ref(), Err(e))),
    };
    let result = match op.as_str() {
        "ping" => Ok(json!({ "pong": true })),
        "auth" => authenticate(s, session, body).await,
        "subscribe" => subscribe(s, session, out, body).await,
        "unsubscribe" => unsubscribe(session, body),
        "place" | "cancel" | "amend" => match order(s, session, out, &op, id.clone(), body) {
            Ok(()) => return None,
            Err(e) => Err(e),
        },
        _ => Err(ApiError::bad_request("unknown op")),
    };
    Some(reply(id.as_ref(), result))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthMessage {
    token: Option<String>,
    key: Option<i64>,
    timestamp: Option<u64>,
    signature: Option<String>,
}

async fn authenticate(
    s: &AppState,
    session: &mut Session,
    body: Map<String, Value>,
) -> Result<Value, ApiError> {
    let m: AuthMessage = parse(body)?;
    let mut headers = HeaderMap::new();
    let value = |v: String| {
        HeaderValue::from_str(&v)
            .map_err(|_| ApiError::unauthorized("credentials contain invalid characters"))
    };
    if let Some(token) = m.token {
        headers.insert("authorization", value(format!("Bearer {token}"))?);
    }
    if let (Some(key), Some(timestamp), Some(signature)) = (m.key, m.timestamp, m.signature) {
        headers.insert("brier-key", value(key.to_string())?);
        headers.insert("brier-timestamp", value(timestamp.to_string())?);
        headers.insert("brier-signature", value(signature)?);
    }
    let user = auth::authenticate(
        &s.db,
        &s.replays,
        &headers,
        &Method::GET,
        "/v1/ws",
        b"",
        true,
    )
    .await?;
    session.user = Some(user);
    Ok(json!({ "user_id": user.id }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelMessage {
    channel: String,
}

fn channel(session: &Session, body: Map<String, Value>) -> Result<Channel, ApiError> {
    let ChannelMessage { channel } = parse(body)?;
    let account = session.user.map(|u| u.account()).transpose()?;
    Channel::parse(&channel, account).map_err(|code| match code {
        "unauthorized" => ApiError::unauthorized("the account channel needs auth first"),
        _ => ApiError::bad_request("channels are book:<market>, trades:<market>, and account"),
    })
}

async fn subscribe(
    s: &AppState,
    session: &mut Session,
    out: &mpsc::Sender<Utf8Bytes>,
    body: Map<String, Value>,
) -> Result<Value, ApiError> {
    let channel = channel(session, body)?;
    if session.subscriptions.contains_key(&channel) {
        return Err(ApiError::bad_request("already subscribed"));
    }
    if session.subscriptions.len() >= MAX_SUBSCRIPTIONS {
        return Err(ApiError::bad_request(format!(
            "at most {MAX_SUBSCRIPTIONS} subscriptions per connection"
        )));
    }
    let subscription = s.feed.subscribe(channel).await.map_err(|code| match code {
        "unknown_market" => ApiError::not_found("no such market"),
        _ => ApiError::internal(&"the market-data publisher stopped"),
    })?;
    let task = tokio::spawn(forward(s.feed.clone(), channel, subscription, out.clone()));
    session.subscriptions.insert(channel, task);
    Ok(json!({ "subscribed": channel.name() }))
}

/// Copy a channel to the connection. A lagging subscriber resubscribes, so it starts again
/// from a fresh first message (on a book channel, a snapshot) rather than silently skipping.
async fn forward(
    feed: FeedHandle,
    channel: Channel,
    mut subscription: Subscription,
    out: mpsc::Sender<Utf8Bytes>,
) {
    loop {
        if out.send(subscription.first.clone()).await.is_err() {
            return;
        }
        loop {
            match subscription.rx.recv().await {
                Ok(message) => {
                    if out.send(message).await.is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(_)) => break,
                Err(RecvError::Closed) => return,
            }
        }
        match feed.subscribe(channel).await {
            Ok(fresh) => subscription = fresh,
            Err(_) => return,
        }
    }
}

fn unsubscribe(session: &mut Session, body: Map<String, Value>) -> Result<Value, ApiError> {
    let channel = channel(session, body)?;
    let task = session
        .subscriptions
        .remove(&channel)
        .ok_or_else(|| ApiError::bad_request("not subscribed"))?;
    task.abort();
    Ok(json!({ "unsubscribed": channel.name() }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OrderMessage {
    order: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AmendMessage {
    order: u64,
    qty: u32,
}

/// Start an order command; its reply goes to the outbox once durable.
fn order(
    s: &Arc<AppState>,
    session: &Session,
    out: &mpsc::Sender<Utf8Bytes>,
    op: &str,
    id: Option<Value>,
    body: Map<String, Value>,
) -> Result<(), ApiError> {
    let user = session
        .user
        .ok_or_else(|| ApiError::unauthorized("orders need auth first"))?;
    let command = order_command(op, body, user.account()?)?;
    if !s.order_limits.allow(user.id) {
        return Err(ApiError::rate_limited());
    }
    let permit = Arc::clone(&session.in_flight)
        .try_acquire_owned()
        .map_err(|_| {
            ApiError::new(
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                "too_many_in_flight",
                format!("at most {MAX_IN_FLIGHT} orders awaiting durability per connection"),
            )
        })?;
    let (state, out) = (Arc::clone(s), out.clone());
    tokio::spawn(async move {
        let result = match crate::leases::submit(&state, command).await {
            Ok(c) => match command {
                Command::Cancel { order, .. } => cancel_result(&c, order),
                Command::Amend { order, .. } => amend_result(&c, order),
                _ => place_result(&c),
            },
            Err(e) => Err(e),
        };
        drop(permit);
        let _ = out.send(reply(id.as_ref(), result)).await; // the connection may have closed
    });
    Ok(())
}
