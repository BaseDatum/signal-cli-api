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

/// In TCP daemon mode, `startLink` sends TWO JSON-RPC responses with the
/// same id on the TCP connection:
///
///   1. The `sgnl://linkdevice?...` URI (returned within seconds)
///   2. The linked account number (after the phone scans — up to minutes)
///
/// We use `rpc_multi` to receive both. Phase 1 is returned to the HTTP
/// caller immediately. A background task keeps the receiver alive for
/// phase 2 (up to 5 minutes). When phase 2 arrives, signal-cli has
/// completed provisioning and written the account to disk.
///
/// IMPORTANT: We must NOT clean up the pending entry after phase 1.
/// Doing so causes the reader_loop to silently drop phase 2, and
/// signal-cli may not complete provisioning if the response can't be
/// delivered.

const PHASE2_TIMEOUT: Duration = Duration::from_secs(5 * 60);

async fn qrcodelink(
    axum::extract::Query(query): axum::extract::Query<QrLinkQuery>,
    State(st): State<AppState>,
) -> Response {
    let mut params = json!({});
    if let Some(name) = query.device_name {
        params["deviceName"] = json!(name);
    }
    match start_link(&st, params).await {
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
    match start_link(&st, params).await {
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

/// Send startLink, grab the URI from response #1, then spawn a
/// background task that waits for response #2 (the account number
/// after the phone scans).
async fn start_link(
    st: &AppState,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let (rpc_id, mut rx) = st.rpc_multi("startLink", params).await?;

    // Phase 1: wait for the URI (first response).
    let first = match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
        Ok(Some(resp)) => resp,
        Ok(None) => {
            st.cleanup_multi_rpc(rpc_id);
            return Err("signal-cli closed the channel before sending URI".to_string());
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

    tracing::info!("startLink phase 1: URI received");

    // Phase 2: spawn a background task that keeps the receiver alive
    // and waits for the account-number response after the phone scans.
    // Do NOT cleanup the pending entry — the reader_loop needs it to
    // route phase 2 to our receiver.
    let st2 = st.clone();
    tokio::spawn(async move {
        tracing::info!(
            "startLink phase 2: waiting up to {:?} for QR scan...",
            PHASE2_TIMEOUT
        );
        match tokio::time::timeout(PHASE2_TIMEOUT, rx.recv()).await {
            Ok(Some(resp)) => {
                if let Some(err) = resp.get("error") {
                    tracing::error!("startLink phase 2 error: {err}");
                } else {
                    tracing::info!("startLink phase 2 complete: {resp}");
                }
            }
            Ok(None) => {
                tracing::warn!("startLink phase 2: channel closed (signal-cli disconnected?)");
            }
            Err(_) => {
                tracing::warn!("startLink phase 2: timed out after {:?}", PHASE2_TIMEOUT);
            }
        }
        // Now clean up the pending entry.
        st2.cleanup_multi_rpc(rpc_id);
    });

    Ok(uri_result)
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
