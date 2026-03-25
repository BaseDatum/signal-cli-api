use crate::state::{Metrics, RpcResponse};
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{broadcast, mpsc, oneshot};

/// Sender that can handle either a single response (oneshot) or multiple
/// responses (mpsc) for the same RPC ID.
pub enum PendingSender {
    Once(oneshot::Sender<RpcResponse>),
    Multi(mpsc::Sender<RpcResponse>),
}

/// Read loop: reads newline-delimited JSON from signal-cli, dispatches responses
/// to pending futures and broadcasts notifications to WebSocket/SSE/webhook clients.
pub async fn reader_loop(
    reader: OwnedReadHalf,
    broadcast_tx: broadcast::Sender<String>,
    pending: Arc<DashMap<u64, PendingSender>>,
    metrics: Arc<Metrics>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let parsed: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Bad JSON from signal-cli: {e}");
                continue;
            }
        };

        // RPC response (has "id" field)
        if let Some(id) = parsed.get("id").and_then(|v| v.as_u64()) {
            if let Some(entry) = pending.get(&id) {
                match entry.value() {
                    PendingSender::Once(_) => {
                        // Remove and send — single-use
                        if let Some((_, PendingSender::Once(tx))) = pending.remove(&id) {
                            let _ = tx.send(parsed);
                        }
                    }
                    PendingSender::Multi(tx) => {
                        // Send but keep the entry for subsequent responses
                        let _ = tx.send(parsed.clone()).await;
                    }
                }
            }
            continue;
        }

        // Notification (incoming message) — broadcast to all listeners
        metrics.inc_received();
        let _ = broadcast_tx.send(line);
    }
    tracing::error!("signal-cli connection closed");
}

/// Dedicated writer loop: serialises all writes through a single task.
pub async fn writer_loop(mut rx: tokio::sync::mpsc::Receiver<String>, mut writer: OwnedWriteHalf) {
    while let Some(line) = rx.recv().await {
        if let Err(e) = writer.write_all(line.as_bytes()).await {
            tracing::error!("Failed to write to signal-cli: {e}");
            break;
        }
        let _ = writer.flush().await;
    }
    tracing::error!("Writer channel closed");
}

/// Send a JSON-RPC request and wait for a single response, with a timeout.
pub async fn rpc_call(
    writer_tx: &tokio::sync::mpsc::Sender<String>,
    pending: &Arc<DashMap<u64, PendingSender>>,
    next_id: &Arc<AtomicU64>,
    method: &str,
    params: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let id = next_id.fetch_add(1, Ordering::Relaxed);

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": id,
    });

    let (tx, rx) = oneshot::channel();
    pending.insert(id, PendingSender::Once(tx));

    let mut line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
    line.push('\n');

    writer_tx.send(line).await.map_err(|e| e.to_string())?;

    let response = match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(_)) => return Err("signal-cli did not respond".to_string()),
        Err(_) => {
            pending.remove(&id);
            return Err(crate::state::RPC_TIMEOUT_ERROR.to_string());
        }
    };

    if let Some(err) = response.get("error") {
        return Err(err.to_string());
    }

    Ok(response
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

/// Send a JSON-RPC request that produces multiple responses (e.g. `startLink`).
///
/// Returns `(id, mpsc::Receiver)` — the caller reads responses from the
/// receiver and MUST call `cleanup_multi(pending, id)` when done.
pub async fn rpc_call_multi(
    writer_tx: &tokio::sync::mpsc::Sender<String>,
    pending: &Arc<DashMap<u64, PendingSender>>,
    next_id: &Arc<AtomicU64>,
    method: &str,
    params: serde_json::Value,
) -> Result<(u64, mpsc::Receiver<RpcResponse>), String> {
    let id = next_id.fetch_add(1, Ordering::Relaxed);

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
        "id": id,
    });

    let (tx, rx) = mpsc::channel(4);
    pending.insert(id, PendingSender::Multi(tx));

    let mut line = serde_json::to_string(&request).map_err(|e| e.to_string())?;
    line.push('\n');

    writer_tx.send(line).await.map_err(|e| e.to_string())?;

    Ok((id, rx))
}

/// Remove a multi-response pending entry (cleanup after startLink completes).
pub fn cleanup_multi(pending: &Arc<DashMap<u64, PendingSender>>, id: u64) {
    pending.remove(&id);
}
