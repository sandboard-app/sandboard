//! sandboard — an agent orchestrator whose board is a control plane, not a report.

mod antigravity;
mod antigravity_oauth;
mod api;
mod auth;
mod db;
mod engine;
mod events;
mod github_app;
mod github_poll;
mod machine;
mod mcp;
mod mcp_client_oauth;
mod mcp_oauth;
mod model;
mod openshell;
mod openshell_oauth;
mod provider_types;
mod cockpit_attach;
mod cockpit_chat;
mod cockpit_mcp;
mod cockpit_mcp_tunnel;
mod mcp_policy;
mod secrets;
mod schema;
mod seed_policies;
mod sse;
mod store;
mod supervisor;
mod ws;

use crate::schema::Schema;
use crate::store::{Board, SharedBoard};

use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

/// Public bootstrap guide for operator agents (`GET /llms.txt`).
const LLMS_TXT: &str = include_str!("../llms.txt");

async fn llms_txt() -> Response {
    (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        LLMS_TXT,
    )
        .into_response()
}

/// How long graceful shutdown waits for open connections (SSE, MCP streams)
/// before we drop them. Without a ceiling, a single Chrome EventSource holds
/// the process forever — Ctrl-C logs "shutting down" and the shell never returns.
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(3);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // OpenShell CLI does the same — rustls 0.23 needs an explicit process-wide
    // CryptoProvider before tonic/reqwest TLS. Harmless if already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sandboard=info,tower_http=warn".into()),
        )
        .init();

    // parking_lot deadlock_detection: poll and log holders instead of hanging
    // forever like std RwLock (the NOT LIVE freeze mode).
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_secs(5));
            let deadlocks = parking_lot::deadlock::check_deadlock();
            if deadlocks.is_empty() {
                continue;
            }
            tracing::error!("{} deadlock(s) detected", deadlocks.len());
            for (i, threads) in deadlocks.iter().enumerate() {
                tracing::error!("deadlock #{i} ({} threads)", threads.len());
                for t in threads {
                    tracing::error!("  thread {:?}\n{:?}", t.thread_id(), t.backtrace());
                }
            }
        }
    });

    // Hierarchy + agent create knobs are compiled defaults. Database URL is
    // process boot only (`SANDBOARD_DATABASE_URL` else sqlite:sandboard.db) — it cannot
    // live on board Settings (Settings persist inside the DB).
    let mut schema = Schema::default();
    db::apply_database_url_override(&mut schema.board.database);
    let json_path = PathBuf::from("sandboard.json");
    let board: SharedBoard = match schema.board.database.parsed() {
        Ok(url) => {
            tracing::info!(%url, backend = %url.backend(), "board database configured");
            let store = Arc::new(
                db::DurableBoardStore::connect(url.as_str())
                    .await
                    .map_err(|e| anyhow::anyhow!("board database open/migrate: {e}"))?,
            );
            Arc::new(
                Board::load_with_store(schema.clone(), json_path, store)
                    .await
                    .map_err(|e| anyhow::anyhow!("board load from database: {e}"))?,
            )
        }
        Err(e) => {
            tracing::warn!("board.database.url invalid ({e}); using sandboard.json");
            Arc::new(Board::load_or_new(schema.clone(), json_path))
        }
    };
    let exec_cfg = schema.execution.clone();

    // Persist on an interval rather than per mutation, so heartbeating agents
    // don't turn into a write storm. Paired with a flush on shutdown, or the
    // last half-second of state is lost on every exit.
    let persist = board.clone();
    {
        let board = board.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            loop {
                tick.tick().await;
                board.flush();
            }
        });
    }

    supervisor::spawn(board.clone(), exec_cfg);

    let web_dist = PathBuf::from("web/dist");
    let mut app = Router::new()
        .nest("/auth", auth::routes())
        .nest("/api", api::routes())
        .route("/api/events", get(sse::events))
        .route("/api/ws", get(ws::ws_handler))
        .route("/healthz", get(|| async { "ok" }))
        .route("/llms.txt", get(llms_txt))
        .nest("/.well-known", mcp_oauth::well_known_routes())
        .nest("/oauth", mcp_oauth::oauth_routes())
        .nest("/oauth/mcp-client", mcp_client_oauth::callback_routes())
        .nest("/oauth/openshell", openshell_oauth::callback_routes())
        .nest("/oauth/antigravity", antigravity_oauth::callback_routes());

    // Operator MCP: Bearer via MCP OAuth once admin exists (bootstrap stays open).
    app = app.nest(
        "/mcp",
        mcp::router(board.clone()).layer(middleware::from_fn_with_state(
            board.clone(),
            mcp_oauth::require_mcp_bearer,
        )),
    );

    if web_dist.exists() {
        app = app.fallback_service(
            ServeDir::new(&web_dist).fallback(ServeFile::new(web_dist.join("index.html"))),
        );
    } else {
        tracing::info!("no web/dist build — run `npm run dev` in web/ for the board UI");
    }

    let app = app
        .layer(middleware::from_fn_with_state(
            board.clone(),
            auth::require_session,
        ))
        // The Vite dev server lives on another origin.
        .layer(CorsLayer::permissive())
        .with_state(board);

    // Overridable so a scratch instance (the UI screenshot harness) can run
    // alongside the real one instead of fighting it for the port. Bind host
    // stays loopback by default; containers set SANDBOARD_BIND_ADDR=0.0.0.0 so the
    // Service can reach the pod.
    let host = std::env::var("SANDBOARD_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("SANDBOARD_PORT").unwrap_or_else(|_| "8080".into());
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("sandboard listening on http://{addr}  (MCP at /mcp)");

    // Graceful shutdown stops accepting, then waits for in-flight connections.
    // Board SSE and MCP streams never close on their own, so we race the drain
    // against a deadline (and a second interrupt) and drop whatever remains.
    let (drain_tx, drain_rx) = tokio::sync::oneshot::channel::<()>();
    let shutting_down = persist.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        wait_interrupt().await;
        tracing::info!("shutting down");
        shutting_down.flush();
        // Returning starts the drain. Signal the watchdog so the deadline
        // starts now, not before the interrupt.
        let _ = drain_tx.send(());
    });

    tokio::select! {
        result = server => result?,
        _ = async {
            let _ = drain_rx.await;
            tokio::select! {
                _ = tokio::time::sleep(SHUTDOWN_DRAIN) => {
                    tracing::warn!(
                        "shutdown drain timed out after {}s; dropping remaining connections",
                        SHUTDOWN_DRAIN.as_secs()
                    );
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::warn!("second interrupt; dropping remaining connections");
                }
            }
        } => {}
    }

    // The interval flusher can be up to its own period behind. Without this,
    // whatever happened in the last half-second is simply lost on exit.
    // (Also covers the force-drop path, where serve never returned cleanly.)
    persist.flush();
    tracing::info!("board flushed; bye");
    Ok(())
}

async fn wait_interrupt() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}

#[cfg(test)]
mod tests {
    #[test]
    fn llms_txt_covers_fresh_board_bootstrap() {
        let body = super::LLMS_TXT;
        assert!(body.contains("/llms.txt"));
        assert!(body.contains("POST /auth/bootstrap"));
        assert!(body.contains("PUT /api/openshell"));
        assert!(body.contains("cursor-agent"));
        assert!(body.contains("PUT /api/github-app"));
        assert!(body.contains("installations"));
        assert!(body.contains("POST /api/sandbox-profiles"));
        assert!(body.contains("POST /api/openshell/policies"));
        assert!(body.contains("clone_repo"));
        // Current contract: walk Settings; do not invent parallel bootstrap.
        assert!(body.contains("Do not invent a parallel bootstrap"));
    }
}
