//! `brier-gateway`: the exchange process (core, journal, REST API, read-model projector).
//!
//! ```text
//! brier-gateway [serve]                 run the exchange
//! brier-gateway promote <user> <reason> give a user the admin role (audit-logged)
//! brier-gateway role <user> <role> <reason>
//!                                       set a user's role: trader, admin, resolver,
//!                                       surveillance, or lister (audit-logged)
//! ```
//! Configuration comes from the environment only: `BRIER_DATABASE_URL` (required; never
//! logged), `BRIER_DATA_DIR` (default `data`), `BRIER_LISTEN` (default `127.0.0.1:8080`),
//! `BRIER_DISPUTE_WINDOW_SECONDS` (default 86400), `BRIER_TOTP_KEY` (64 hex digits; the key
//! TOTP secrets are encrypted under; without it TOTP and admin actions are unavailable),
//! `BRIER_PROXY_KEY` (optional, at least 32 characters: the web server sends it with each
//! request so the gateway trusts the client address it forwards; without it forwarded
//! addresses are ignored), and `BRIER_DATABASE_SCHEMA` (optional: keep
//! every table in this schema, created if missing, so several exchanges, such as test runs,
//! can share one database).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use brier_core::CoreConfig;
use brier_gateway::{AppState, DISPUTE_WINDOW, router, run_projector, scheduler};
use serde_json::json;
use tokio::net::TcpListener;
use tokio_postgres::{Client, NoTls};

fn info(msg: &str, detail: &serde_json::Value) {
    eprintln!(
        "{}",
        json!({ "level": "info", "msg": msg, "detail": detail })
    );
}

async fn connect(url: &str) -> Result<Client, String> {
    let (client, connection) = tokio_postgres::connect(url, NoTls)
        .await
        .map_err(|e| format!("database: {}", brier_readmodel::describe_db_error(&e)))?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!(
                "{}",
                json!({ "level": "error", "msg": "database connection closed", "detail": e.to_string() })
            );
        }
    });
    if let Ok(schema) = std::env::var("BRIER_DATABASE_SCHEMA") {
        let valid = (1..=63).contains(&schema.len())
            && schema
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
            && schema
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !valid {
            return Err("BRIER_DATABASE_SCHEMA must match ^[a-z_][a-z0-9_]{0,62}$".to_owned());
        }
        // Safe to interpolate: the name was just checked against a closed character set.
        client
            .batch_execute(&format!(
                "CREATE SCHEMA IF NOT EXISTS {schema}; SET search_path TO {schema};"
            ))
            .await
            .map_err(|e| format!("database: {}", brier_readmodel::describe_db_error(&e)))?;
    }
    Ok(client)
}

async fn shutdown_signal() {
    let interrupt = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        else {
            let _ = interrupt.await;
            return;
        };
        tokio::select! {
            _ = interrupt => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = interrupt.await;
}

/// The TOTP key and the proxy key, from the environment.
fn with_keys(mut state: AppState) -> Result<AppState, String> {
    match std::env::var("BRIER_TOTP_KEY") {
        Ok(hex) => {
            let key = brier_gateway::totp::parse_key(&hex)
                .ok_or("BRIER_TOTP_KEY must be 64 lowercase hex digits")?;
            state = state.with_totp_key(key);
        }
        Err(_) => info(
            "no BRIER_TOTP_KEY: TOTP and admin actions are unavailable",
            &json!({}),
        ),
    }
    match std::env::var("BRIER_PROXY_KEY") {
        Ok(key) if key.chars().count() >= 32 => state = state.with_proxy_key(&key),
        Ok(_) => return Err("BRIER_PROXY_KEY must be at least 32 characters".to_owned()),
        Err(_) => info(
            "no BRIER_PROXY_KEY: client addresses behind the web server are not known",
            &json!({}),
        ),
    }
    Ok(state)
}

async fn serve(url: &str) -> Result<(), String> {
    let data = PathBuf::from(std::env::var("BRIER_DATA_DIR").unwrap_or_else(|_| "data".to_owned()));
    let listen: SocketAddr = std::env::var("BRIER_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_owned())
        .parse()
        .map_err(|e| format!("BRIER_LISTEN: {e}"))?;
    let dispute_window = match std::env::var("BRIER_DISPUTE_WINDOW_SECONDS") {
        Ok(v) => v
            .parse::<i64>()
            .ok()
            .filter(|w| (1..=30 * 86_400).contains(w))
            .ok_or("BRIER_DISPUTE_WINDOW_SECONDS must be 1 to 2592000")?,
        Err(_) => DISPUTE_WINDOW,
    };
    let mut db = connect(url).await?;
    brier_readmodel::migrate(&mut db)
        .await
        .map_err(|e| e.to_string())?;
    let projector_db = connect(url).await?;

    let (core, recovery, core_thread) =
        brier_core::start(CoreConfig::new(&data)).map_err(|e| e.to_string())?;
    info(
        "recovered",
        &json!({ "next_seq": recovery.next_seq.get(), "replayed": recovery.replayed, "snapshot": recovery.snapshot.map(brier_types::Seq::get) }),
    );
    let mut state = AppState::new(core.clone(), db)
        .map_err(|e| format!("{e:?}"))?
        .with_dispute_window(dispute_window);
    state = with_keys(state)?;
    // Test builds only: a looser sign-in limit, for browser tests that sign many users in from
    // one address.
    #[cfg(feature = "test-clock")]
    if let Some(n) = std::env::var("BRIER_TEST_AUTH_PER_MINUTE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    {
        info("test sign-in limit", &json!({ "per_minute": n }));
        state.auth_limits = brier_gateway::limits::Limiter::per_minute(n, n);
    }
    #[cfg(feature = "test-clock")]
    let state = match std::env::var("BRIER_TEST_CLOCK_FILE") {
        Ok(path) => {
            info("test clock", &json!({ "file": path }));
            state.with_clock(brier_gateway::clock::Clock::from_file(PathBuf::from(path)))
        }
        Err(_) => state,
    };
    let state = Arc::new(state);
    let schedule = tokio::spawn(scheduler::run(Arc::clone(&state)));
    let leases = tokio::spawn(brier_gateway::leases::run(Arc::clone(&state)));
    let projector = tokio::spawn(run_projector(
        projector_db,
        data,
        core.durable(),
        Arc::clone(&state.projected),
    ));
    drop(core);

    let listener = TcpListener::bind(listen)
        .await
        .map_err(|e| format!("bind {listen}: {e}"))?;
    info("listening", &json!({ "address": listen.to_string() }));
    axum::serve(
        listener,
        router(Arc::clone(&state)).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        // WebSocket connections are not HTTP requests the server waits for; close them.
        state.shutdown.send_replace(true);
    })
    .await
    .map_err(|e| e.to_string())?;

    // The scheduler stops on the same signal; with it and the router gone, every core handle
    // is gone, so the core finishes its last batch and stops.
    schedule.await.map_err(|e| format!("scheduler task: {e}"))?;
    leases.await.map_err(|e| format!("order lease task: {e}"))?;
    let core_result = tokio::task::spawn_blocking(move || core_thread.join())
        .await
        .map_err(|e| e.to_string())?;
    core_result
        .map_err(|_| "the core thread panicked".to_owned())?
        .map_err(|e| e.to_string())?;
    match projector.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(format!("projector: {e}")),
        Err(e) => return Err(format!("projector task: {e}")),
    }
    info("stopped", &json!({}));
    Ok(())
}

fn db_err(e: &tokio_postgres::Error) -> String {
    format!("database: {}", brier_readmodel::describe_db_error(e))
}

async fn promote(url: &str, username: &str, reason: &str) -> Result<(), String> {
    set_role(url, username, "admin", reason).await
}

async fn set_role(url: &str, username: &str, role: &str, reason: &str) -> Result<(), String> {
    if !["trader", "admin", "resolver", "surveillance", "lister"].contains(&role) {
        return Err("role must be trader, admin, resolver, surveillance, or lister".to_owned());
    }
    if !(3..=500).contains(&reason.trim().chars().count()) {
        return Err("reason must be 3 to 500 characters".to_owned());
    }
    let mut db = connect(url).await?;
    brier_readmodel::migrate(&mut db)
        .await
        .map_err(|e| e.to_string())?;
    let tx = db.transaction().await.map_err(|e| db_err(&e))?;
    let row = tx
        .query_opt(
            "UPDATE users SET role = $2 WHERE username = $1 RETURNING id",
            &[&username, &role],
        )
        .await
        .map_err(|e| db_err(&e))?
        .ok_or_else(|| format!("no user named {username}"))?;
    let id: i64 = row.get(0);
    tx.execute(
        "INSERT INTO audit_log (user_id, action, detail, reason) VALUES ($1, 'set_role', $2::text::jsonb, $3)",
        &[&id, &json!({ "via": "cli", "role": role }).to_string(), &reason.trim()],
    )
    .await
    .map_err(|e| db_err(&e))?;
    tx.commit().await.map_err(|e| db_err(&e))?;
    info("role set", &json!({ "user_id": id, "role": role }));
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Ok(url) = std::env::var("BRIER_DATABASE_URL") else {
        eprintln!("set BRIER_DATABASE_URL (for local development: make db-url)");
        return ExitCode::FAILURE;
    };
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["serve"] => serve(&url).await,
        ["promote", username, reason] => promote(&url, username, reason).await,
        ["role", username, role, reason] => set_role(&url, username, role, reason).await,
        _ => Err(
            "usage: brier-gateway [serve] | promote <username> <reason> | role <username> <trader|admin|resolver> <reason>"
                .to_owned(),
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "{}",
                json!({ "level": "error", "msg": "brier-gateway failed", "detail": e })
            );
            ExitCode::FAILURE
        }
    }
}
