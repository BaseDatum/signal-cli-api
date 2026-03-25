use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

use crate::state::AppState;
use super::helpers::{rpc_ok, rpc_no_content};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/qrcodelink", get(qrcodelink))
        .route("/v1/qrcodelink/raw", get(qrcodelink_raw))
        .route("/v1/devices/{number}", post(link_device).get(list_devices))
        .route("/v1/devices/{number}/{device_id}", delete(remove_device))
        .route(
            "/v1/devices/{number}/local-data",
            delete(delete_local_data),
        )
}

#[derive(Deserialize)]
struct QrLinkQuery {
    #[serde(default)]
    device_name: Option<String>,
}

/// Default timeout for finishLink (5 minutes — user needs time to scan).
const FINISH_LINK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// In HTTP daemon mode, device linking is two separate RPCs
/// (confirmed by DjinnBot's working implementation):
///
///   1. `startLink` — instant, returns `{ "deviceLinkUri": "sgnl://..." }`
///   2. `finishLink` — blocks up to 5 minutes waiting for phone to scan
///
/// We return the URI to the caller immediately, then spawn finishLink
/// in a background task. The admin UI polls /v1/accounts to detect
/// when linking completes.

async fn qrcodelink(
    axum::extract::Query(query): axum::extract::Query<QrLinkQuery>,
    State(st): State<AppState>,
) -> Response {
    let device_name = query.device_name.clone().unwrap_or_else(|| "signal-cli".to_string());

    // Step 1: startLink — returns the URI.
    let uri_result = match st.rpc("startLink", json!({})).await {
        Ok(result) => result,
        Err(e) => {
            let status = crate::state::rpc_error_status(&e);
            return (status, Json(json!({ "error": e }))).into_response();
        }
    };

    let uri = uri_result
        .as_str()
        .or_else(|| uri_result.get("deviceLinkUri").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();

    if uri.is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "startLink did not return a URI" })),
        )
            .into_response();
    }

    tracing::info!("startLink returned URI, spawning finishLink background task");

    // Step 2: finishLink in background (blocks until phone scans).
    spawn_finish_link(st, uri.clone(), device_name);

    Json(json!({ "deviceLinkUri": uri })).into_response()
}

async fn qrcodelink_raw(
    axum::extract::Query(query): axum::extract::Query<QrLinkQuery>,
    State(st): State<AppState>,
) -> Response {
    let device_name = query.device_name.clone().unwrap_or_else(|| "signal-cli".to_string());

    let uri_result = match st.rpc("startLink", json!({})).await {
        Ok(result) => result,
        Err(e) => {
            let status = crate::state::rpc_error_status(&e);
            return (status, Json(json!({ "error": e }))).into_response();
        }
    };

    let uri = uri_result
        .as_str()
        .or_else(|| uri_result.get("deviceLinkUri").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();

    if uri.is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "startLink did not return a URI",
        )
            .into_response();
    }

    tracing::info!("startLink returned URI, spawning finishLink background task");

    spawn_finish_link(st, uri.clone(), device_name);

    (StatusCode::OK, uri).into_response()
}

/// Spawn a background task that calls `finishLink` and waits for the phone
/// to scan the QR code (up to 5 minutes).
fn spawn_finish_link(st: AppState, uri: String, device_name: String) {
    tokio::spawn(async move {
        tracing::info!("finishLink: waiting for QR scan (timeout {:?})", FINISH_LINK_TIMEOUT);

        let params = json!({
            "deviceLinkUri": uri,
            "deviceName": device_name,
        });

        let result = st.rpc_with_timeout("finishLink", params, FINISH_LINK_TIMEOUT).await;

        match result {
            Ok(resp) => {
                tracing::info!("finishLink succeeded: {resp}");
            }
            Err(e) => {
                tracing::error!("finishLink failed: {e}");
            }
        }
    });
}

#[derive(Deserialize)]
struct LinkDeviceBody {
    uri: String,
    #[serde(default)]
    device_name: Option<String>,
}

async fn link_device(
    Path(number): Path<String>,
    State(st): State<AppState>,
    Json(body): Json<LinkDeviceBody>,
) -> Response {
    let mut params = json!({ "account": number, "uri": body.uri });
    if let Some(name) = body.device_name {
        params["deviceName"] = json!(name);
    }
    rpc_no_content(&st, "finishLink", params).await
}

async fn list_devices(Path(number): Path<String>, State(st): State<AppState>) -> Response {
    rpc_ok(&st, "listDevices", json!({ "account": number })).await
}

async fn remove_device(
    Path((number, device_id)): Path<(String, i64)>,
    State(st): State<AppState>,
) -> Response {
    rpc_no_content(&st, "removeDevice", json!({ "account": number, "deviceId": device_id })).await
}

async fn delete_local_data(Path(number): Path<String>, State(st): State<AppState>) -> Response {
    rpc_no_content(&st, "deleteLocalAccountData", json!({ "account": number })).await
}
