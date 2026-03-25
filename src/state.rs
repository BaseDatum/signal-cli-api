use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, RwLock};

use crate::jsonrpc::PendingSender;

pub type RpcResponse = serde_json::Value;

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Metrics {
    pub messages_sent: AtomicU64,
    pub messages_received: AtomicU64,
    pub rpc_calls: AtomicU64,
    pub rpc_errors: AtomicU64,
    pub ws_clients: AtomicU64,
}

impl Metrics {
    pub fn inc_sent(&self) {
        self.messages_sent.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_received(&self) {
        self.messages_received.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_rpc(&self) {
        self.rpc_calls.fetch_add(1, Ordering::Relaxed);
    }
    pub fn inc_rpc_error(&self) {
        self.rpc_errors.fetch_add(1, Ordering::Relaxed);
    }
    pub fn to_prometheus(&self) -> String {
        format!(
            "# HELP signal_messages_sent_total Total messages sent\n\
             # TYPE signal_messages_sent_total counter\n\
             signal_messages_sent_total {}\n\
             # HELP signal_messages_received_total Total messages received\n\
             # TYPE signal_messages_received_total counter\n\
             signal_messages_received_total {}\n\
             # HELP signal_rpc_calls_total Total JSON-RPC calls to signal-cli\n\
             # TYPE signal_rpc_calls_total counter\n\
             signal_rpc_calls_total {}\n\
             # HELP signal_rpc_errors_total Total JSON-RPC errors\n\
             # TYPE signal_rpc_errors_total counter\n\
             signal_rpc_errors_total {}\n\
             # HELP signal_ws_clients_active Active WebSocket clients\n\
             # TYPE signal_ws_clients_active gauge\n\
             signal_ws_clients_active {}\n",
            self.messages_sent.load(Ordering::Relaxed),
            self.messages_received.load(Ordering::Relaxed),
            self.rpc_calls.load(Ordering::Relaxed),
            self.rpc_errors.load(Ordering::Relaxed),
            self.ws_clients.load(Ordering::Relaxed),
        )
    }
}

// ---------------------------------------------------------------------------
// Webhook
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WebhookConfig {
    pub id: String,
    pub url: String,
    #[serde(default)]
    pub events: Vec<String>, // empty = all events
}

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AppState {
    pub writer_tx: tokio::sync::mpsc::Sender<String>,
    pub broadcast_tx: broadcast::Sender<String>,
    pub pending: Arc<DashMap<u64, PendingSender>>,
    pub next_id: Arc<AtomicU64>,
    pub metrics: Arc<Metrics>,
    pub webhooks: Arc<RwLock<Vec<WebhookConfig>>>,
    pub rpc_timeout: Duration,
    /// Set to `false` when the TCP connection to signal-cli daemon dies.
    /// Checked by `/v1/health` (so k8s liveness probe restarts us) and
    /// by `rpc()`/`rpc_multi()` (so callers fail fast instead of waiting
    /// 30 seconds for a timeout on a dead pipe).
    pub daemon_alive: Arc<AtomicBool>,
}

/// Sentinel error string returned when an RPC call times out.
pub const RPC_TIMEOUT_ERROR: &str = "RPC_TIMEOUT";

/// Sentinel error string returned when the daemon connection is dead.
pub const DAEMON_DEAD_ERROR: &str = "DAEMON_DEAD";

/// Map an RPC error string to the appropriate HTTP status code.
pub fn rpc_error_status(err: &str) -> axum::http::StatusCode {
    if err == RPC_TIMEOUT_ERROR {
        axum::http::StatusCode::GATEWAY_TIMEOUT
    } else if err == DAEMON_DEAD_ERROR {
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    } else {
        axum::http::StatusCode::BAD_REQUEST
    }
}

impl AppState {
    pub fn new(writer_tx: tokio::sync::mpsc::Sender<String>) -> Self {
        let (broadcast_tx, _) = broadcast::channel(256);
        Self {
            writer_tx,
            broadcast_tx,
            pending: Arc::new(DashMap::new()),
            next_id: Arc::new(AtomicU64::new(1)),
            metrics: Arc::new(Metrics::default()),
            webhooks: Arc::new(RwLock::new(Vec::new())),
            rpc_timeout: Duration::from_secs(30),
            daemon_alive: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Returns true if the signal-cli daemon TCP connection is alive.
    pub fn is_daemon_alive(&self) -> bool {
        self.daemon_alive.load(Ordering::Relaxed)
    }

    /// Helper: make a JSON-RPC call to signal-cli (single response).
    ///
    /// Fails immediately with `DAEMON_DEAD` if the connection is known
    /// to be down, instead of waiting 30 s for a timeout.
    pub async fn rpc(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        if !self.is_daemon_alive() {
            self.metrics.inc_rpc();
            self.metrics.inc_rpc_error();
            return Err(DAEMON_DEAD_ERROR.to_string());
        }
        self.metrics.inc_rpc();
        let result = crate::jsonrpc::rpc_call(
            &self.writer_tx,
            &self.pending,
            &self.next_id,
            method,
            params,
            self.rpc_timeout,
        )
        .await;
        if result.is_err() {
            self.metrics.inc_rpc_error();
        }
        result
    }

    /// Start a multi-response JSON-RPC call (e.g. `startLink`).
    ///
    /// Returns `(rpc_id, Receiver)`. The caller reads responses from the
    /// receiver and MUST call `cleanup_multi_rpc(rpc_id)` when done.
    pub async fn rpc_multi(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(u64, mpsc::Receiver<RpcResponse>), String> {
        if !self.is_daemon_alive() {
            self.metrics.inc_rpc();
            self.metrics.inc_rpc_error();
            return Err(DAEMON_DEAD_ERROR.to_string());
        }
        self.metrics.inc_rpc();
        crate::jsonrpc::rpc_call_multi(
            &self.writer_tx,
            &self.pending,
            &self.next_id,
            method,
            params,
        )
        .await
    }

    /// Clean up a multi-response pending entry.
    pub fn cleanup_multi_rpc(&self, id: u64) {
        crate::jsonrpc::cleanup_multi(&self.pending, id);
    }
}
