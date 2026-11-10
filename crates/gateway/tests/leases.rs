//! Account isolation, durable deadlines and REST/WS submission guards on a real core/database.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::too_many_lines
)]
mod common;

use axum::http::{Method, StatusCode};
use brier_core::{CoreConfig, start};
use brier_engine::{Command, MarketState};
use brier_gateway::clock::Clock;
use brier_gateway::{AppState, leases, router};
use brier_types::{AccountId, MarketId};
use common::{call, connect, login};
use serde_json::json;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs PostgreSQL (BRIER_TEST_DATABASE_URL)"]
async fn leases_cancel_only_their_owner_and_survive_a_new_gateway_state() {
    let schema = common::schema("leases");
    let data = tempfile::tempdir().unwrap();
    let (core, _, _thread) = start(CoreConfig::new(data.path())).unwrap();
    let clock = Clock::manual(1_794_240_000);
    let state = Arc::new(
        AppState::new(core.clone(), connect(&schema).await)
            .unwrap()
            .with_clock(clock.clone()),
    );
    let app = router(Arc::clone(&state));
    let mut tokens = Vec::new();
    for name in ["maker", "other"] {
        let (status, body) = call(&app, Method::POST, "/v1/register", None,
            Some(json!({"username": name, "password": "long enough password", "adult": true, "terms": "2026-11-08"}))).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        tokens.push(login(&app, name, "long enough password").await);
    }
    core.submit(Command::CreateMarket {
        market: MarketId::new(1),
        rules_hash: None,
    })
    .await
    .unwrap();
    core.submit(Command::SetState {
        market: MarketId::new(1),
        state: MarketState::Open,
    })
    .await
    .unwrap();
    let order = json!({"market": 1, "side": "buy", "price": 40, "qty": 5});
    let (status, _) = call(
        &app,
        Method::POST,
        "/v1/orders/cancel-after",
        None,
        Some(json!({"seconds": 5})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    for seconds in [1, 301] {
        let (status, _) = call(
            &app,
            Method::POST,
            "/v1/orders/cancel-after",
            Some(&tokens[0]),
            Some(json!({"seconds": seconds})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    let (status, _) = call(
        &app,
        Method::POST,
        "/v1/orders/cancel-after",
        Some(&tokens[0]),
        Some(json!({"seconds": 5})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for token in &tokens {
        assert_eq!(
            call(
                &app,
                Method::POST,
                "/v1/orders",
                Some(token),
                Some(order.clone())
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    clock.set(clock.now() + 5);
    // Expiry blocks placement even before the timer has performed cancellation.
    let (status, body) = call(
        &app,
        Method::POST,
        "/v1/orders",
        Some(&tokens[0]),
        Some(order.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "lease_expired");
    let mine = call(&app, Method::GET, "/v1/portfolio", Some(&tokens[0]), None)
        .await
        .1;
    let order_id = mine["open_orders"][0]["order"].as_u64().unwrap();
    let (status, body) = call(
        &app,
        Method::PATCH,
        &format!("/v1/orders/{order_id}"),
        Some(&tokens[0]),
        Some(json!({"qty": 4})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "lease_expired");
    // The same entry point protects WebSocket commands.
    let command =
        brier_gateway::routes::decode_order(order.to_string().as_bytes(), AccountId::new(1))
            .unwrap();
    assert!(leases::submit(&state, command).await.is_err());
    let restarted = Arc::new(
        AppState::new(core.clone(), connect(&schema).await)
            .unwrap()
            .with_clock(clock.clone()),
    );
    assert_eq!(leases::expire(&restarted).await.unwrap(), 1);
    assert_eq!(leases::expire(&restarted).await.unwrap(), 0);
    let audit = restarted
        .db
        .query_one(
            "SELECT actor FROM audit_log WHERE reason = 'maker heartbeat expired'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(audit.get::<_, &str>(0), "system");
    let app = router(Arc::clone(&restarted));
    let mine = call(&app, Method::GET, "/v1/portfolio", Some(&tokens[0]), None)
        .await
        .1;
    let other = call(&app, Method::GET, "/v1/portfolio", Some(&tokens[1]), None)
        .await
        .1;
    assert_eq!(mine["open_orders"].as_array().unwrap().len(), 0);
    assert_eq!(other["open_orders"].as_array().unwrap().len(), 1);
    assert_eq!(
        call(
            &app,
            Method::POST,
            "/v1/orders/cancel-after",
            Some(&tokens[0]),
            Some(json!({"seconds": 5}))
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            Method::POST,
            "/v1/orders",
            Some(&tokens[0]),
            Some(order)
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            Method::POST,
            "/v1/orders/cancel-after",
            Some(&tokens[0]),
            Some(json!({"seconds": 0}))
        )
        .await
        .0,
        StatusCode::OK
    );
    let mine = call(&app, Method::GET, "/v1/portfolio", Some(&tokens[0]), None)
        .await
        .1;
    assert_eq!(mine["open_orders"].as_array().unwrap().len(), 0);
    assert_eq!(
        call(&app, Method::DELETE, "/v1/orders", Some(&tokens[1]), None)
            .await
            .0,
        StatusCode::OK
    );
}
