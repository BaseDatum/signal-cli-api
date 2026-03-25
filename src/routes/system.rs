use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/about", get(about))
}

/// Returns 204 if the signal-cli daemon connection is alive, 503 otherwise.
/// The k8s liveness probe uses this to restart the container when the
/// daemon connection dies.
async fn health(State(st): State<AppState>) -> Response {
    if st.is_daemon_alive() {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "signal-cli daemon connection lost").into_response()
    }
}

async fn about() -> Response {
    let info = json!({
        "versions": {
            "signal-cli-api": env!("CARGO_PKG_VERSION"),
        },
        "build": {
            "target": std::env::consts::ARCH,
            "os": std::env::consts::OS,
        }
    });
    Json(info).into_response()
}
