mod api;
pub mod app;
pub mod common;
pub mod domain;
pub mod publishers;
pub mod sources;
mod utils;
use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::signal;
use utils::DVR_DIRECTORY;

use crate::app::GlobalState;
fn init() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    ffmpeg::init()?;
    gstreamer::init()?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    init()?;
    log::info!("Starting media server v{}", env!("CARGO_PKG_VERSION"));

    if let Err(e) = utils::check_dependencies() {
        log::error!("Dependency check failed: {}", e);
        std::process::exit(1);
    }

    // Create DVR directory if it doesn't exist
    std::fs::create_dir_all(DVR_DIRECTORY)?;
    log::info!("DVR directory: {}", DVR_DIRECTORY);

    let app_state = Arc::new(GlobalState::new().await?);
    let app = api::create_router(app_state.clone());

    let mut port = 8009;
    let args: Vec<String> = std::env::args().collect();
    for i in 0..args.len() {
        if args[i] == "--port" && i + 1 < args.len() {
            if let Ok(p) = args[i + 1].parse::<u16>() {
                port = p;
            }
        }
    }

    let addr = std::env::var("BIND_ADDRESS").unwrap_or_else(|_| format!("0.0.0.0:{}", port));
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    log::info!("Server listening on {}", addr);
    log::info!("Swagger UI available at http://{}/swagger-ui", addr);
    log::info!(
        "WebRTC Test Client available at http://{}/debug/webrtc-client",
        addr
    );


    // Clone the stream manager to be able to clean up on shutdown
    let app_state_clone = Arc::clone(&app_state);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    // Perform cleanup after server shutdown
    log::info!("Performing cleanup...");
    if let Err(e) = app_state_clone.cleanup().await {
        log::error!("Error during cleanup: {}", e);
    }
    log::info!("Server shutdown complete");

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            log::info!("Received Ctrl+C signal");
        },
        _ = terminate => {
            log::info!("Received terminate signal");
        },
    }
}
