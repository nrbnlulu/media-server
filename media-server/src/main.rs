mod api;
pub mod app;
pub mod common;
pub mod domain;
mod options;
pub mod publishers;
pub mod sources;
mod utils;
use anyhow::Result;
use clap::Parser;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::signal;
use tracing_subscriber::{EnvFilter, fmt::writer::MakeWriterExt};

use crate::app::GlobalState;
fn init(options: &options::Options) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let file_appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("media-server.log")
        .build(&options.log_dir)?;
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = EnvFilter::try_new(&options.log_filter)?;

    tracing_subscriber::fmt()
        .with_writer(std::io::stdout.and(non_blocking))
        .with_env_filter(env_filter)
        .init();

    ffmpeg::init()?;
    gstreamer::init()?;

    // Verify fallback placeholder files exist
    let placeholder_files = ["assets/placeholder_h264.ts", "assets/placeholder_h265.ts"];
    for file_path in &placeholder_files {
        if !std::path::Path::new(file_path).exists() {
            anyhow::bail!(
                "Fallback placeholder file not found: {}. This file is required for RTSP fallback streaming.",
                file_path
            );
        }
    }

    Ok(guard)
}

#[tokio::main]
async fn main() -> Result<()> {
    let options = options::Options::parse();
    domain::dvr::filesystem::configure_base_dir(options.dvr_dir.clone())?;
    let _guard = init(&options)?;
    log::info!("Starting media server v{}", env!("CARGO_PKG_VERSION"));

    if let Err(e) = utils::check_dependencies() {
        log::error!("Dependency check failed: {}", e);
        std::process::exit(1);
    }

    let addr = options.address();
    let listener = tokio::net::TcpListener::bind(addr).await?;

    let addr = listener.local_addr()?;

    // Create DVR directory if it doesn't exist
    std::fs::create_dir_all(&options.dvr_dir)?;
    log::info!("DVR directory: {}", options.dvr_dir.display());

    let recovery_dir = options.dvr_dir.clone();
    let recovered =
        tokio::task::spawn_blocking(move || domain::dvr::maintenance::recover(&recovery_dir))
            .await??;
    log::info!("Recovered {} DVR segments", recovered);
    let maintenance = options.retention_seconds().map(|seconds| {
        let directory = options.dvr_dir.clone();
        let interval_seconds = options.retention_interval_secs;
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(interval_seconds));
            loop {
                interval.tick().await;
                let directory = directory.clone();
                match tokio::task::spawn_blocking(move || {
                    domain::dvr::maintenance::retain(
                        &directory,
                        utils::get_current_unix_timestamp(),
                        seconds,
                    )
                })
                .await
                {
                    Ok(Ok(count)) => {
                        if count > 0 {
                            log::info!("Removed {} expired DVR segments", count);
                        }
                    }
                    result => log::error!("DVR retention failed: {:?}", result),
                }
            }
        })
    });
    let app_state = Arc::new(GlobalState::new().await?);
    let app = api::create_router(app_state.clone());

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

    if let Some(maintenance) = maintenance {
        maintenance.abort();
        let _ = maintenance.await;
    }
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
