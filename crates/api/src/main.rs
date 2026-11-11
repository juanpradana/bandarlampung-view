//! CCTV API — unified camera dataset + stream proxies.

mod proxy;
mod routes;
mod state;

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::state::AppState;

#[derive(Parser, Debug)]
#[command(version, about = "Bandar Lampung unified CCTV API")]
struct Args {
    /// Directory holding cameras.json / cameras.geojson
    #[arg(short, long, default_value = "static/data")]
    data_dir: PathBuf,

    /// Directory holding the built WASM client (index.html, js/, css/)
    #[arg(short, long, default_value = "static")]
    static_dir: PathBuf,

    /// Bind address. Override with `CCTV_BIND`, e.g. `0.0.0.0:3447` in a
    /// container, where the loopback default would be unreachable from outside.
    #[arg(short, long, env = "CCTV_BIND", default_value = "127.0.0.1:3447")]
    bind: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info")),
        )
        .init();

    let args = Args::parse();

    let state = AppState::load(&args.data_dir).context("load dataset")?;

    tracing::info!(
        cameras = state.dataset().cameras.len(),
        "dataset loaded"
    );

    let index = args.static_dir.join("index.html");
    let static_service = ServeDir::new(&args.static_dir)
        .not_found_service(ServeFile::new(&index));

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = routes::router(state.clone())
        .fallback_service(static_service)
        .layer(CompressionLayer::new())
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr: SocketAddr = args.bind.parse().context("parse bind address")?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on http://{addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received");
}