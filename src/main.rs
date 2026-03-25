mod daemon;
mod jsonrpc;
mod middleware;
mod routes;
mod state;
mod webhooks;

use axum::middleware as axum_mw;
use clap::Parser;
use std::net::SocketAddr;
use tower_http::cors::CorsLayer;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "signal-cli-api", about = "REST + WebSocket API for signal-cli")]
struct Cli {
    /// Connect to an existing signal-cli HTTP daemon at this URL
    /// (e.g. http://127.0.0.1:8820).
    /// If omitted, signal-cli is auto-spawned as a child process.
    #[arg(long)]
    signal_cli: Option<String>,

    /// Listen address for HTTP API
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: String,

    /// Path to TLS certificate file (PEM format). Enables HTTPS when set.
    #[arg(long)]
    tls_cert: Option<String>,

    /// Path to TLS private key file (PEM format). Required with --tls-cert.
    #[arg(long)]
    tls_key: Option<String>,
}

#[tokio::main(worker_threads = 4)]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();

    // Either connect to an existing daemon or auto-spawn one.
    let _managed_daemon; // held alive so child process isn't dropped
    let signal_cli_url = match cli.signal_cli {
        Some(url) => url,
        None => {
            let d = daemon::spawn().await?;
            let url = d.base_url.clone();
            _managed_daemon = d;
            url
        }
    };

    tracing::info!("Using signal-cli HTTP daemon at {signal_cli_url}");
    let app_state = state::AppState::new(signal_cli_url.clone());

    // Spawn SSE reader for incoming message notifications.
    let sse_state = app_state.clone();
    tokio::spawn(jsonrpc::sse_reader_loop(sse_state));

    // Spawn webhook dispatcher
    let webhook_state = app_state.clone();
    tokio::spawn(webhooks::dispatch_loop(webhook_state));

    let app = routes::router(app_state)
        .layer(axum_mw::from_fn(middleware::request_tracing))
        .layer(CorsLayer::permissive());

    let requested: SocketAddr = cli.listen.parse()?;

    match (cli.tls_cert, cli.tls_key) {
        (Some(cert), Some(key)) => {
            let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key).await?;
            let addr = match tokio::net::TcpListener::bind(requested).await {
                Ok(probe) => { drop(probe); requested }
                Err(_) => {
                    let fallback = SocketAddr::from(([127, 0, 0, 1], 0));
                    let probe = tokio::net::TcpListener::bind(fallback).await?;
                    let addr = probe.local_addr()?;
                    drop(probe);
                    tracing::warn!("Port {} busy, using {addr} instead", requested.port());
                    addr
                }
            };
            tracing::info!("Listening on https://{addr} (TLS)");
            tokio::select! {
                result = axum_server::bind_rustls(addr, tls_config)
                    .serve(app.into_make_service()) => { result?; }
                _ = shutdown_signal() => {
                    tracing::info!("Shutdown signal received, stopping...");
                }
            }
        }
        (None, None) => {
            let listener = match tokio::net::TcpListener::bind(requested).await {
                Ok(l) => l,
                Err(_) => {
                    let fallback = SocketAddr::from(([127, 0, 0, 1], 0));
                    let l = tokio::net::TcpListener::bind(fallback).await?;
                    tracing::warn!(
                        "Port {} busy, using {} instead",
                        requested.port(),
                        l.local_addr()?
                    );
                    l
                }
            };
            tracing::info!("Listening on http://{}", listener.local_addr()?);
            tokio::select! {
                result = axum::serve(listener, app) => { result?; }
                _ = shutdown_signal() => {
                    tracing::info!("Shutdown signal received, stopping...");
                }
            }
        }
        _ => {
            anyhow::bail!("Both --tls-cert and --tls-key must be provided together");
        }
    }

    // _managed_daemon drops here → process group killed
    Ok(())
}

/// Wait for SIGTERM or Ctrl+C, whichever comes first.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {}
        _ = sigterm.recv() => {}
    }
}
