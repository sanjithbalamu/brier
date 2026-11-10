//! Durable, account-scoped cancel-after leases for unattended market makers.
//!
//! Expiry blocks new REST and WebSocket placements and cancels resting orders. Renewing an
//! expired lease cancels first, so old quotes cannot be resurrected by a late heartbeat.
//! Deadlines survive restart in PostgreSQL; resulting cancels are committed to the journal.
#![allow(clippy::missing_errors_doc)]

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use brier_core::Committed;
use brier_engine::Command;
use brier_types::AccountId;
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::audit::{self, Actor};
use crate::error::ApiError;
use crate::routes::{ApiResult, Req, Shared};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Heartbeat {
    seconds: u32,
}

async fn cancel(s: &AppState, user: i64, actor: Actor, reason: &str) -> Result<u64, ApiError> {
    let account = AccountId::new(u64::try_from(user).map_err(|e| ApiError::internal(&e))?);
    let done = audit::submit(
        s,
        actor,
        "cancel_account_orders",
        Command::CancelAll {
            account: Some(account),
            market: None,
        },
        json!({"account": user}),
        reason,
    )
    .await?;
    Ok(done.committed.seq.get())
}

/// Arm/renew the caller's lease for 5..300 seconds. Zero cancels orders and disarms it.
pub async fn heartbeat(State(s): Shared, req: Req) -> ApiResult {
    let user = req.user(&s, true).await?;
    if !user.role.trades() {
        return Err(ApiError::forbidden());
    }
    if !s.order_limits.allow(user.id) {
        return Err(ApiError::rate_limited());
    }
    let Heartbeat { seconds } = req.json()?;
    if seconds != 0 && !(5..=300).contains(&seconds) {
        return Err(ApiError::bad_request(
            "seconds must be 0 (cancel and disarm) or 5..300",
        ));
    }
    let _guard = s.order_safety.write().await;
    let now = s.clock.now();
    let previous =
        s.db.query_opt(
            "SELECT expires_at FROM order_leases WHERE user_id = $1",
            &[&user.id],
        )
        .await?;
    let overdue = previous.is_some_and(|r| r.get::<_, i64>(0) <= now);
    if seconds == 0 || overdue {
        cancel(
            &s,
            user.id,
            Actor::User(user.id),
            "maker lease disarmed or renewed after expiry",
        )
        .await?;
    }
    if seconds == 0 {
        s.db.execute("DELETE FROM order_leases WHERE user_id = $1", &[&user.id])
            .await?;
        return Ok(Json(json!({"armed": false, "expires_at": null})).into_response());
    }
    let deadline = now
        .checked_add(i64::from(seconds))
        .ok_or_else(|| ApiError::bad_request("deadline overflow"))?;
    s.db.execute(
        "INSERT INTO order_leases(user_id, expires_at, cancelled) VALUES($1, $2, false) \
        ON CONFLICT(user_id) DO UPDATE SET expires_at = EXCLUDED.expires_at, cancelled = false",
        &[&user.id, &deadline],
    )
    .await?;
    Ok(Json(json!({"armed": true, "expires_at": deadline})).into_response())
}

/// Cancel the caller's orders without administrative permissions. The lease stays armed.
pub async fn cancel_mine(State(s): Shared, req: Req) -> ApiResult {
    let user = req.user(&s, true).await?;
    if !user.role.trades() {
        return Err(ApiError::forbidden());
    }
    if !s.order_limits.allow(user.id) {
        return Err(ApiError::rate_limited());
    }
    let _guard = s.order_safety.write().await;
    let seq = cancel(
        &s,
        user.id,
        Actor::User(user.id),
        "account requested cancellation of its orders",
    )
    .await?;
    Ok(Json(json!({"seq": seq, "status": "cancelled"})).into_response())
}

/// Submit an order while excluding concurrent lease expiry. Cancels remain available.
pub async fn submit(s: &AppState, command: Command) -> Result<Committed, ApiError> {
    let guarded_account = match command {
        Command::Place(order) => Some(order.account),
        Command::Amend { account, .. } => Some(account),
        _ => None,
    };
    if let Some(owner) = guarded_account {
        let _guard = s.order_safety.read().await;
        let account = i64::try_from(owner.get()).map_err(|e| ApiError::internal(&e))?;
        if let Some(row) =
            s.db.query_opt(
                "SELECT expires_at FROM order_leases WHERE user_id = $1",
                &[&account],
            )
            .await?
            && row.get::<_, i64>(0) <= s.clock.now()
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "lease_expired",
                "renew the cancellation lease before placing or amending an order",
            ));
        }
        return Ok(s.core.submit(command).await?);
    }
    Ok(s.core.submit(command).await?)
}

/// Cancel all due accounts once. A failed command is retried; the row is retained on failure.
pub async fn expire(s: &AppState) -> Result<usize, ApiError> {
    let _guard = s.order_safety.write().await;
    let rows = s.db.query("SELECT user_id FROM order_leases WHERE expires_at <= $1 AND NOT cancelled ORDER BY user_id", &[&s.clock.now()]).await?;
    for row in &rows {
        let user: i64 = row.get(0);
        cancel(s, user, Actor::System, "maker heartbeat expired").await?;
        s.db.execute(
            "UPDATE order_leases SET cancelled = true WHERE user_id = $1",
            &[&user],
        )
        .await?;
    }
    Ok(rows.len())
}

/// Run separately from data fetching, market scheduling and scoring, until gateway shutdown.
pub async fn run(s: Arc<AppState>) {
    let mut shutdown = s.shutdown.subscribe();
    loop {
        if let Err(e) = expire(&s).await {
            crate::log(
                "error",
                "order lease cancellation failed; retrying",
                &e.body(),
            );
        }
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(250)) => {}
            _ = shutdown.wait_for(|stop| *stop) => return,
        }
    }
}
