use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};
use uuid::Uuid;

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
    pub fn inc_sent(&self) { self.messages_sent.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_received(&self) { self.messages_received.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_rpc(&self) { self.rpc_calls.fetch_add(1, Ordering::Relaxed); }
    pub fn inc_rpc_error(&self) { self.rpc_errors.fetch_add(1, Ordering::Relaxed); }

    pub fn to_prometheus(&self) -> String {
        format!(
            "# HELP signal_messages_sent_total Total messages sent\n\
             # TYPE signal_messages_sent_total counter\n\
             signal_messages_sent_total {}\n\
             # HELP signal_messages_received_total Total messages received\n\
             # TYPE signal_messages_received_total counter\n\
             signal_messages_received_total {}\n\
             # HELP signal_rpc_calls_total Total RPC calls\n\
             # TYPE signal_rpc_calls_total counter\n\
             signal_rpc_calls_total {}\n\
             # HELP signal_rpc_errors_total Total RPC errors\n\
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
// Webhook config
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct WebhookConfig {
    #[serde(default)]
    pub id: String,
    pub url: String,
    #[serde(default)]
    pub events: Vec<String>,
}

// ---------------------------------------------------------------------------
// AppState — HTTP-mode RPC to signal-cli
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AppState {
    /// HTTP client for JSON-RPC calls to signal-cli's built-in HTTP daemon.
    pub http_client: reqwest::Client,
    /// Base URL of signal-cli's HTTP daemon (e.g. "http://127.0.0.1:12345").
    pub signal_cli_url: String,
    /// Broadcast channel for incoming message notifications (fed by SSE reader).
    pub broadcast_tx: broadcast::Sender<String>,
    pub metrics: Arc<Metrics>,
    pub webhooks: Arc<RwLock<Vec<WebhookConfig>>>,
    pub rpc_timeout: Duration,
    /// Set to `false` when the SSE connection to signal-cli dies.
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
    pub fn new(signal_cli_url: String) -> Self {
        let (broadcast_tx, _) = broadcast::channel(256);
        Self {
            http_client: reqwest::Client::new(),
            signal_cli_url,
            broadcast_tx,
            metrics: Arc::new(Metrics::default()),
            webhooks: Arc::new(RwLock::new(Vec::new())),
            rpc_timeout: Duration::from_secs(30),
            daemon_alive: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Returns true if the signal-cli daemon connection is alive.
    pub fn is_daemon_alive(&self) -> bool {
        self.daemon_alive.load(Ordering::Relaxed)
    }

    /// Make a JSON-RPC call to signal-cli via HTTP POST to /api/v1/rpc.
    ///
    /// Returns the `result` field on success, or an error string on failure.
    pub async fn rpc(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        self.rpc_with_timeout(method, params, self.rpc_timeout).await
    }

    /// Like `rpc()` but with a custom timeout (e.g. 5 minutes for finishLink).
    pub async fn rpc_with_timeout(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        if !self.is_daemon_alive() {
            self.metrics.inc_rpc();
            self.metrics.inc_rpc_error();
            return Err(DAEMON_DEAD_ERROR.to_string());
        }
        self.metrics.inc_rpc();

        let id = Uuid::new_v4().to_string();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": id,
        });

        let url = format!("{}/api/v1/rpc", self.signal_cli_url);

        let resp = self.http_client
            .post(&url)
            .json(&body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| {
                self.metrics.inc_rpc_error();
                if e.is_timeout() {
                    RPC_TIMEOUT_ERROR.to_string()
                } else {
                    format!("HTTP request failed: {e}")
                }
            })?;

        // signal-cli returns 201 for void methods (no response body).
        if resp.status().as_u16() == 201 {
            return Ok(serde_json::Value::Null);
        }

        let text = resp.text().await.map_err(|e| {
            self.metrics.inc_rpc_error();
            format!("Failed to read response body: {e}")
        })?;

        if text.is_empty() {
            return Ok(serde_json::Value::Null);
        }

        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            self.metrics.inc_rpc_error();
            format!("Malformed JSON from signal-cli: {e}")
        })?;

        if let Some(err) = parsed.get("error") {
            self.metrics.inc_rpc_error();
            return Err(err.to_string());
        }

        Ok(parsed.get("result").cloned().unwrap_or(serde_json::Value::Null))
    }
}
