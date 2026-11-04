//! Local HTTP API (`rustplay serve`). Compiled only with the `server` feature.
//!
//! - Binds 127.0.0.1 by default; a non-loopback bind requires tokens.
//! - Bearer tokens map to a principal with a role (layer 05).
//! - A semaphore caps concurrent browsers; excess requests get 429 instead of
//!   queueing unboundedly (layer 07 burst control).
//! - Each task runs in its own tokio task, so a panic becomes a 500 and the
//!   session's Drop still kills Chromium.

use crate::access::{Principal, TokenTable};
use crate::config::SessionConfig;
use crate::errors::{ErrorReport, RpError};
use crate::task::{run_task, Task};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Semaphore;

use crate::TASK_SCHEMA;

pub struct ServeOptions {
    pub addr: SocketAddr,
    pub max_sessions: usize,
    pub tokens: TokenTable,
}

struct AppState {
    cfg: SessionConfig,
    sem: Arc<Semaphore>,
    max: usize,
    tokens: TokenTable,
    served: AtomicU64,
    rejected_busy: AtomicU64,
}

pub fn router(cfg: SessionConfig, max_sessions: usize, tokens: TokenTable) -> Router {
    let state = Arc::new(AppState {
        cfg,
        sem: Arc::new(Semaphore::new(max_sessions)),
        max: max_sessions,
        tokens,
        served: AtomicU64::new(0),
        rejected_busy: AtomicU64::new(0),
    });
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/schema", get(|| async { ([("content-type", "application/json")], TASK_SCHEMA) }))
        .route("/v1/tasks", post(run))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .with_state(state)
}

pub async fn serve(cfg: SessionConfig, opts: ServeOptions) -> Result<(), RpError> {
    if !opts.addr.ip().is_loopback() && opts.tokens.is_empty() {
        return Err(RpError::config(
            "refusing to bind a non-loopback address without auth; pass --tokens-file or --token-env",
        ));
    }
    if opts.max_sessions == 0 {
        return Err(RpError::config("--max-sessions must be >= 1"));
    }
    let listener =
        tokio::net::TcpListener::bind(opts.addr).await.map_err(|e| RpError::io(format!("bind {}", opts.addr), e))?;
    tracing::info!(addr = %opts.addr, max_sessions = opts.max_sessions, auth = !opts.tokens.is_empty(), "rustplay API listening");
    eprintln!("rustplay API on http://{}  (POST /v1/tasks, GET /healthz)", opts.addr);
    axum::serve(listener, router(cfg, opts.max_sessions, opts.tokens))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down; waiting for in-flight tasks");
        })
        .await
        .map_err(|e| RpError::io("serve", e))
}

async fn health(State(st): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "active_sessions": st.max - st.sem.available_permits(),
        "max_sessions": st.max,
        "tasks_served": st.served.load(Ordering::Relaxed),
        "rejected_busy": st.rejected_busy.load(Ordering::Relaxed),
    }))
}

fn err(status: StatusCode, e: &RpError) -> Response {
    (status, Json(json!({ "ok": false, "error": e.report() }))).into_response()
}

fn err_report(status: StatusCode, r: ErrorReport) -> Response {
    (status, Json(json!({ "ok": false, "error": r }))).into_response()
}

fn authenticate(st: &AppState, headers: &HeaderMap) -> Option<Principal> {
    if st.tokens.is_empty() {
        return Some(Principal { name: "local".into(), role: st.cfg.role });
    }
    let h = headers.get("authorization")?.to_str().ok()?;
    let tok = h.strip_prefix("Bearer ")?;
    st.tokens.authenticate(tok.trim())
}

async fn run(State(st): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(principal) = authenticate(&st, &headers) else {
        return err_report(
            StatusCode::UNAUTHORIZED,
            ErrorReport {
                code: "UNAUTHORIZED",
                message: "missing or invalid bearer token".into(),
                hint: Some("send `Authorization: Bearer <token>`".into()),
                retryable: false,
            },
        );
    };
    let text = match std::str::from_utf8(&body) {
        Ok(t) => t,
        Err(_) => return err(StatusCode::BAD_REQUEST, &RpError::config("body is not UTF-8")),
    };
    let task = match Task::from_json(text) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, &e),
    };
    let Ok(permit) = st.sem.clone().try_acquire_owned() else {
        st.rejected_busy.fetch_add(1, Ordering::Relaxed);
        return err_report(
            StatusCode::TOO_MANY_REQUESTS,
            ErrorReport {
                code: "BUSY",
                message: format!("all {} browser slots are in use", st.max),
                hint: Some("retry with backoff, or raise --max-sessions if the host has RAM to spare".into()),
                retryable: true,
            },
        );
    };
    let mut cfg = st.cfg.clone();
    cfg.role = principal.role;
    tracing::info!(principal = %principal.name, role = principal.role.name(), steps = task.steps.len(), "task accepted");
    let handle = tokio::spawn(async move {
        let r = run_task(&task, &cfg).await;
        drop(permit);
        r
    });
    st.served.fetch_add(1, Ordering::Relaxed);
    match handle.await {
        Ok(report) => {
            let status = match report.error.as_ref().map(|e| e.code) {
                Some("FORBIDDEN") => StatusCode::FORBIDDEN,
                Some("CONFIG_INVALID" | "POLICY_VIOLATION") if report.session_id.is_none() => StatusCode::BAD_REQUEST,
                _ => StatusCode::OK,
            };
            (status, Json(report)).into_response()
        }
        Err(e) => {
            tracing::error!("task panicked: {e}");
            err_report(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorReport {
                    code: "INTERNAL",
                    message: "task runner panicked; browser was killed".into(),
                    hint: None,
                    retryable: true,
                },
            )
        }
    }
}
