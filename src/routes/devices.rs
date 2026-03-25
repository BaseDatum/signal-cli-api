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

/// Helper: initiate startLink, return the URI from the first response,
/// and immediately release the RPC so other calls aren't blocked.
///
/// signal-cli's `startLink` JSON-RPC sends TWO responses with the same ID:
/// 1. The `sgnl://linkdevice?...` URI (returned immediately)
/// 2. The linked account number (after the phone scans the QR)
///
/// signal-cli's daemon processes RPCs sequentially on a single TCP
/// connection. If we keep the pending entry alive waiting for phase 2,
/// ALL other RPCs (listAccounts, send, health, etc.) will block.
///
/// Instead, we grab the URI from phase 1, immediately clean up the
/// pending entry, and let signal-cli handle phase 2 internally. The
/// account will still be written to disk when the phone scans — we
/// just won't get a notification about it. The admin UI polls
/// `/v1/accounts` to detect when linking completes.
async fn start_link_two_phase(
    st: &AppState,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (rpc_id, mut rx) = st.rpc_multi("startLink", params).await?;

    // Phase 1: wait for the URI (first response).
    let first = match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
        Ok(Some(resp)) => resp,
        Ok(None) => {
            st.cleanup_multi_rpc(rpc_id);
            return Err("signal-cli closed the connection".to_string());
        }
        Err(_) => {
            st.cleanup_multi_rpc(rpc_id);
            return Err("Timeout waiting for startLink URI".to_string());
        }
    };

    if let Some(err) = first.get("error") {
        st.cleanup_multi_rpc(rpc_id);
        return Err(err.to_string());
    }

    let uri_result = first
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    // Immediately clean up the pending entry so signal-cli's daemon
    // isn't blocked waiting for us to consume phase 2. Signal-cli will
    // still complete the provisioning internally and write the account
    // to disk. The phase 2 response (with the account number) will be
    // treated as a notification by the reader loop and broadcast to
    // any WebSocket/SSE/webhook listeners.
    st.cleanup_multi_rpc(rpc_id);
    tracing::info!("startLink phase 1 complete — URI returned, pending entry released");

    Ok(uri_result)
}

async fn qrcodelink(
    axum::extract::Query(query): axum::extract::Query<QrLinkQuery>,
    State(st): State<AppState>,
) -> Response {
    let mut params = json!({});
    if let Some(name) = query.device_name {
        params["deviceName"] = json!(name);
    }
    match start_link_two_phase(&st, params).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => {
            let status = crate::state::rpc_error_status(&e);
            (status, Json(json!({ "error": e }))).into_response()
        }
    }
}

async fn qrcodelink_raw(
    axum::extract::Query(query): axum::extract::Query<QrLinkQuery>,
    State(st): State<AppState>,
) -> Response {
    let mut params = json!({});
    if let Some(name) = query.device_name {
        params["deviceName"] = json!(name);
    }
    match start_link_two_phase(&st, params).await {
        Ok(result) => {
            let uri = result
                .as_str()
                .or_else(|| result.get("deviceLinkUri").and_then(|v| v.as_str()))
                .unwrap_or("");
            (StatusCode::OK, uri.to_string()).into_response()
        }
        Err(e) => {
            let status = crate::state::rpc_error_status(&e);
            (status, Json(json!({ "error": e }))).into_response()
        }
    }
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
