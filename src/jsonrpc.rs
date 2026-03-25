use crate::state::{Metrics, RpcResponse};
use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// Lightweight tag so we can drop the DashMap Ref (and its read lock)
/// before acting on the pending entry.
enum SenderKind {
    Once,
    Multi,
}

/// Read loop: reads newline-delimited JSON from signal-cli, dispatches responses
/// to pending futures and broadcasts notifications to WebSocket/SSE/webhook clients.
///
/// Sets `daemon_alive` to `false` on exit so that `/v1/health` returns 503
/// and RPC callers fail fast instead of waiting for timeouts.
pub async fn reader_loop(
    reader: OwnedReadHalf,
    broadcast_tx: broadcast::Sender<String>,
    pending: Arc<DashMap<u64, PendingSender>>,
    metrics: Arc<Metrics>,
    daemon_alive: Arc<AtomicBool>,
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
        //
        // IMPORTANT: We must NOT hold a DashMap Ref (read lock) while
        // calling remove() (write lock) on the same shard — that's an
        // instant deadlock with parking_lot's non-reentrant RwLock.
        // Similarly, holding a Ref across an .await point blocks
        // writers on other tasks from accessing the same shard.
        //
        // Strategy: peek at the entry type, drop the Ref immediately,
        // then act on the type without holding any lock.
        if let Some(id) = parsed.get("id").and_then(|v| v.as_u64()) {
            // Peek at what kind of sender is registered, then drop the
            // read lock BEFORE doing anything else.
            let sender_kind = pending.get(&id).map(|entry| match entry.value() {
                PendingSender::Once(_) => SenderKind::Once,
                PendingSender::Multi(_) => SenderKind::Multi,
            });
            // `entry` (Ref) is dropped here — read lock released.

            match sender_kind {
                Some(SenderKind::Once) => {
                    // Now safe to acquire the write lock.
                    if let Some((_, PendingSender::Once(tx))) = pending.remove(&id) {
                        let _ = tx.send(parsed);
                    }
                }
                Some(SenderKind::Multi) => {
                    // Re-acquire a read lock briefly to clone the sender,
                    // then drop it before the async send.
                    let tx = pending.get(&id).and_then(|entry| {
                        if let PendingSender::Multi(tx) = entry.value() {
                            Some(tx.clone())
                        } else {
                            None
                        }
                    });
                    if let Some(tx) = tx {
                        let _ = tx.send(parsed.clone()).await;
                    }
                }
                None => {
                    // No pending entry — stale or already-cleaned-up response.
                }
            }
            continue;
        }

        // Notification (incoming message) — broadcast to all listeners
        metrics.inc_received();
        let _ = broadcast_tx.send(line);
    }
    tracing::error!("signal-cli connection closed — marking daemon as dead");
    daemon_alive.store(false, Ordering::Relaxed);
}

/// Dedicated writer loop: serialises all writes through a single task.
///
/// Sets `daemon_alive` to `false` on exit.
pub async fn writer_loop(
    mut rx: tokio::sync::mpsc::Receiver<String>,
    mut writer: OwnedWriteHalf,
    daemon_alive: Arc<AtomicBool>,
) {
    while let Some(line) = rx.recv().await {
        if let Err(e) = writer.write_all(line.as_bytes()).await {
            tracing::error!("Failed to write to signal-cli: {e}");
            break;
        }
        let _ = writer.flush().await;
    }
    tracing::error!("Writer channel closed — marking daemon as dead");
    daemon_alive.store(false, Ordering::Relaxed);
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
