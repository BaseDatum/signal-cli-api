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

/// Timeout for the second phase of startLink (waiting for phone to scan).
const LINK_COMPLETION_TIMEOUT: Duration = Duration::from_secs(120);

/// Helper: initiate startLink, return the URI from the first response,
/// and spawn a background task to wait for the second response
/// (provisioning completion).
///
/// signal-cli's `startLink` JSON-RPC sends TWO responses with the same ID:
/// 1. The `sgnl://linkdevice?...` URI (returned immediately)
/// 2. The linked account number (after the phone scans the QR)
///
/// We return the URI to the HTTP caller right away. A background tokio task
/// keeps the RPC alive so the reader loop can deliver the second response
/// to signal-cli, which finalizes the account registration.
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

    // Phase 2: spawn a background task to wait for the linking completion.
    // This keeps the mpsc entry alive so the reader_loop can deliver the
    // second response to signal-cli, which finalizes account registration.
    let st_clone = st.clone();
    tokio::spawn(async move {
        match tokio::time::timeout(LINK_COMPLETION_TIMEOUT, rx.recv()).await {
            Ok(Some(resp)) => {
                if let Some(err) = resp.get("error") {
                    tracing::warn!("startLink phase 2 error: {err}");
                } else {
                    let account = resp
                        .get("result")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    tracing::info!("startLink completed — account linked: {account}");
                }
            }
            Ok(None) => {
                tracing::warn!("startLink phase 2: channel closed before completion");
            }
            Err(_) => {
                tracing::warn!(
                    "startLink phase 2 timed out after {}s — user may not have scanned the QR",
                    LINK_COMPLETION_TIMEOUT.as_secs()
                );
            }
        }
        st_clone.cleanup_multi_rpc(rpc_id);
    });

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
