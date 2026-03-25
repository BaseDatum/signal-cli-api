//! SSE event reader for signal-cli's HTTP daemon.
//!
//! Replaces the TCP reader_loop. Connects to signal-cli's SSE endpoint
//! at `/api/v1/events` and broadcasts incoming messages to WebSocket/SSE
//! clients and webhooks.

use crate::state::AppState;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Connect to signal-cli's SSE event stream and broadcast incoming
/// notifications. Reconnects automatically on disconnect.
pub async fn sse_reader_loop(st: AppState) {
    let url = format!("{}/api/v1/events", st.signal_cli_url);
    let mut backoff = Duration::from_secs(1);

    loop {
        tracing::info!("Connecting to signal-cli SSE stream: {url}");

        match st.http_client.get(&url)
            .header("Accept", "text/event-stream")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!("SSE stream connected");
                st.daemon_alive.store(true, Ordering::Relaxed);
                backoff = Duration::from_secs(1); // reset on success

                // Read the SSE stream line by line.
                use futures_util::StreamExt;
                let mut stream = resp.bytes_stream();
                let mut buffer = String::new();
                let mut current_data = String::new();

                while let Some(chunk) = stream.next().await {
                    let chunk = match chunk {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!("SSE stream read error: {e}");
                            break;
                        }
                    };

                    buffer.push_str(&String::from_utf8_lossy(&chunk));

                    // Process complete lines.
                    while let Some(newline_pos) = buffer.find('\n') {
                        let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
                        buffer = buffer[newline_pos + 1..].to_string();

                        if line.is_empty() {
                            // Empty line = end of event. Dispatch if we have data.
                            if !current_data.is_empty() {
                                st.metrics.inc_received();
                                let _ = st.broadcast_tx.send(current_data.clone());
                                current_data.clear();
                            }
                        } else if let Some(data) = line.strip_prefix("data:") {
                            let data = data.strip_prefix(' ').unwrap_or(data);
                            if current_data.is_empty() {
                                current_data = data.to_string();
                            } else {
                                current_data.push('\n');
                                current_data.push_str(data);
                            }
                        }
                        // Ignore event:, id:, comment lines
                    }
                }

                tracing::warn!("SSE stream ended");
            }
            Ok(resp) => {
                tracing::warn!("SSE connect failed: HTTP {}", resp.status());
            }
            Err(e) => {
                tracing::warn!("SSE connect error: {e}");
            }
        }

        st.daemon_alive.store(false, Ordering::Relaxed);
        tracing::info!("Reconnecting SSE in {:?}...", backoff);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}
