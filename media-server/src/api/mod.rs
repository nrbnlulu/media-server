pub mod handlers;
pub mod models;

use axum::{
    extract::DefaultBodyLimit,
    routing::{delete, get, post},
    Router,
};
use std::sync::Arc;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::app::GlobalState;

#[derive(OpenApi)]
#[openapi(
    paths(
        handlers::health,
        handlers::create_stream,
        handlers::list_streams,
        handlers::get_stream,
        handlers::delete_stream,
        handlers::create_webrtc_session,
        handlers::create_dvr_webrtc_session,
        handlers::delete_webrtc_session,
        handlers::list_webrtc_sessions,
        handlers::webrtc_client,
        handlers::seek_session,
        handlers::set_session_speed,
        handlers::switch_session_to_live,
        handlers::get_session_mode,
    ),
    components(
        schemas(
            models::CreateStreamRequest,
            models::CreateStreamResponse,
            models::StreamResponse,
            models::ListStreamsResponse,
            models::CreateWebRtcSessionRequest,
            models::CreateDvrWebRtcSessionRequest,
            models::CreateWebRtcSessionResponse,
            models::WebRtcSessionResponse,
            models::ListWebRtcSessionsResponse,
            models::HealthResponse,
            models::ErrorResponse,
            models::SeekRequest,
            models::SpeedRequest,
            models::SessionModeResponse,
        )
    ),
    tags(
        (name = "Health", description = "Health check endpoints"),
        (name = "Streams", description = "RTSP stream management endpoints"),
        (name = "WebRTC", description = "WebRTC session management endpoints"),
        (name = "Debug", description = "Debug and testing utilities")
    ),
    info(
        title = "Media Server API",
        version = "0.1.0",
        description = "A high-performance media server that proxies RTSP camera streams to WebRTC clients with optional DVR recording",
        contact(
            name = "Media Server",
        )
    )
)]
pub struct ApiDoc;

pub fn create_router(state: Arc<GlobalState>) -> Router {
    let api_router = Router::new()
        .route("/health", get(handlers::health))
        .route("/streams", post(handlers::create_stream))
        .route("/streams", get(handlers::list_streams))
        .route("/streams/:source_id", get(handlers::get_stream))
        .route("/streams/:source_id", delete(handlers::delete_stream))
        .route("/streams/:source_id/wsc-rtp", get(handlers::wsc_rtp))
        .route(
            "/streams/:source_id/webrtc",
            post(handlers::create_webrtc_session),
        )
        .route(
            "/streams/:source_id/webrtc/dvr",
            post(handlers::create_dvr_webrtc_session),
        )
        .route(
            "/streams/:source_id/webrtc/:session_id",
            delete(handlers::delete_webrtc_session),
        )
        .route(
            "/streams/:source_id/webrtc",
            get(handlers::list_webrtc_sessions),
        )
        .route(
            "/streams/:source_id/webrtc/:session_id/seek",
            post(handlers::seek_session),
        )
        .route(
            "/streams/:source_id/webrtc/:session_id/speed",
            post(handlers::set_session_speed),
        )
        .route(
            "/streams/:source_id/webrtc/:session_id/live",
            post(handlers::switch_session_to_live),
        )
        .route(
            "/streams/:source_id/webrtc/:session_id/mode",
            get(handlers::get_session_mode),
        )
        // WSC-RTP session control endpoints (same as WebRTC but for wsc-rtp sessions)
        .route(
            "/streams/:source_id/wsc-rtp/:session_id/seek",
            post(handlers::seek_session),
        )
        .route(
            "/streams/:source_id/wsc-rtp/:session_id/speed",
            post(handlers::set_session_speed),
        )
        .route(
            "/streams/:source_id/wsc-rtp/:session_id/live",
            post(handlers::switch_session_to_live),
        )
        .route(
            "/streams/:source_id/wsc-rtp/:session_id/mode",
            get(handlers::get_session_mode),
        )
        .route("/debug/webrtc-client", get(handlers::webrtc_client))
        .with_state(state)
        .layer(DefaultBodyLimit::max(10 * 1024 * 1024))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    Router::new()
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .merge(api_router)
}
