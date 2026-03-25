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

/// Device linking is a two-RPC flow (learned from DjinnBot's working impl):
///
/// 1. `startLink` — instant, returns `{ "deviceLinkUri": "sgnl://..." }`
/// 2. `finishLink` — blocks until the phone scans the QR (up to 5 minutes)
///
/// The `/v1/qrcodelink` endpoints call `startLink`, return the URI to the
/// caller, then spawn `finishLink` as a background task. The admin UI polls
/// `/v1/accounts` to detect when linking completes.

/// Default timeout for finishLink (5 minutes — user needs time to scan).
const FINISH_LINK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

async fn qrcodelink(
    axum::extract::Query(query): axum::extract::Query<QrLinkQuery>,
    State(st): State<AppState>,
) -> Response {
    let device_name = query.device_name.clone().unwrap_or_else(|| "signal-cli".to_string());

    // Step 1: startLink — get the URI.
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

    // Step 2: spawn finishLink in the background.
    spawn_finish_link(st, uri.clone(), device_name);

    Json(json!({ "deviceLinkUri": uri })).into_response()
}

async fn qrcodelink_raw(
    axum::extract::Query(query): axum::extract::Query<QrLinkQuery>,
    State(st): State<AppState>,
) -> Response {
    let device_name = query.device_name.clone().unwrap_or_else(|| "signal-cli".to_string());

    // Step 1: startLink — get the URI.
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

    // Step 2: spawn finishLink in the background.
    spawn_finish_link(st, uri.clone(), device_name);

    (StatusCode::OK, uri).into_response()
}

/// Spawn a background task that calls `finishLink` and waits for the phone
/// to scan the QR code. This blocks on the signal-cli RPC for up to 5
/// minutes. On success, signal-cli writes the account to disk and
/// `listAccounts` will return it.
fn spawn_finish_link(st: AppState, uri: String, device_name: String) {
    tokio::spawn(async move {
        tracing::info!("finishLink: waiting for QR scan (timeout {:?})", FINISH_LINK_TIMEOUT);

        let params = json!({
            "deviceLinkUri": uri,
            "deviceName": device_name,
        });

        // Use a dedicated long-timeout RPC call (the normal 30s timeout is
        // way too short — the user needs minutes to scan).
        let result = crate::jsonrpc::rpc_call(
            &st.writer_tx,
            &st.pending,
            &st.next_id,
            "finishLink",
            params,
            FINISH_LINK_TIMEOUT,
        )
        .await;

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
