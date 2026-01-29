use crate::api::{models::*, GlobalState};
use crate::app::ClientSessionId;
use crate::domain::StreamConfig;
use crate::publishers::wsc_rtp::handle_incoming_wsc_trp_websocket;
use axum::extract::ws::WebSocketUpgrade;
use axum::{
    extract::{ConnectInfo, Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Json,
};
use media_server_api_models::CreateWebRtcSessionRequest;
use std::net::SocketAddr;
use std::sync::Arc;
use uuid::Uuid;

pub enum ApiError {
    NotFound(String),
    BadRequest(String),
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };

        let error_response = ErrorResponse {
            error: status.to_string(),
            message,
        };

        (status, Json(error_response)).into_response()
    }
}

#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "Server is healthy", body = HealthResponse)
    ),
    tag = "Health"
)]
pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "healthy".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

#[utoipa::path(
    post,
    path = "/streams",
    request_body = CreateStreamRequest,
    responses(
        (status = 201, description = "Stream created successfully", body = CreateStreamResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse)
    ),
    tag = "Streams"
)]
pub async fn create_stream(
    State(state): State<Arc<GlobalState>>,
    Json(req): Json<CreateStreamRequest>,
) -> Result<(StatusCode, Json<CreateStreamResponse>), ApiError> {
    let source_id = req.source_id;

    let rtsp_url = req
        .rtsp_url
        .parse::<url::Url>()
        .map_err(|e| ApiError::BadRequest(format!("Invalid RTSP URL: {}", e)))?;

    crate::sources::rtsp::RtspClient::validate_rtsp_url(rtsp_url.as_str())
        .map_err(|e| ApiError::BadRequest(format!("Invalid RTSP URL: {}", e)))?;

    let config = StreamConfig {
        source_id: req.source_id,
        rtsp_url,
        username: req.username,
        password: req.password,
        should_record: req.should_record,
        restart_interval_secs: req.restart_interval_secs,
    };

    state.create_rtsp_stream(config).await.map_err(|e| {
        let error_msg = e.to_string();
        if error_msg.contains("already exists") {
            ApiError::BadRequest(error_msg)
        } else {
            ApiError::BadRequest(error_msg)
        }
    })?;

    log::info!("Created stream {}: {}", source_id, req.rtsp_url);

    Ok((
        StatusCode::CREATED,
        Json(CreateStreamResponse {
            source_id,
            rtsp_url: req.rtsp_url,
            should_record: req.should_record,
            restart_interval_secs: req.restart_interval_secs,
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/streams",
    responses(
        (status = 200, description = "List of all streams", body = ListStreamsResponse)
    ),
    tag = "Streams"
)]
pub async fn list_streams(State(state): State<Arc<GlobalState>>) -> Json<ListStreamsResponse> {
    let streams = state.list_streams().await;
    let streams = streams
        .into_iter()
        .map(|s| StreamResponse {
            source_id: s.session_id,
            rtsp_url: s.rtsp_url,
            state: format!("{:?}", s.state),
            should_record: s.should_record,
            restart_interval_secs: s.restart_interval_secs,
            recording_start_time: s.recording_start_time
        })
        .collect();
    Json(ListStreamsResponse { streams })
}

#[utoipa::path(
    get,
    path = "/streams/{source_id}",
    params(
        ("source_id" = u64, Path, description = "Stream unique identifier")
    ),
    responses(
        (status = 200, description = "Stream details", body = StreamResponse),
        (status = 404, description = "Stream not found", body = ErrorResponse)
    ),
    tag = "Streams"
)]
pub async fn get_stream(
    State(state): State<Arc<GlobalState>>,
    Path(source_id): Path<u64>,
) -> Result<Json<StreamResponse>, ApiError> {
    let info = state
        .get_stream(source_id)
        .await
        .map_err(|_| ApiError::NotFound(format!("Stream {} not found", source_id)))?;

    Ok(Json(StreamResponse {
        source_id: info.session_id,
        rtsp_url: info.rtsp_url,
        state: format!("{:?}", info.state),
        should_record: info.should_record,
        restart_interval_secs: info.restart_interval_secs,
        recording_start_time: info.recording_start_time
    }))
}

#[utoipa::path(
    delete,
    path = "/streams/{source_id}",
    params(
        ("source_id" = u64, Path, description = "Stream unique identifier")
    ),
    responses(
        (status = 204, description = "Stream deleted successfully"),
        (status = 404, description = "Stream not found", body = ErrorResponse)
    ),
    tag = "Streams"
)]
pub async fn delete_stream(
    State(state): State<Arc<GlobalState>>,
    Path(source_id): Path<u64>,
) -> Result<StatusCode, ApiError> {
    state
        .delete_stream(source_id)
        .await
        .map_err(|e| ApiError::NotFound(e.to_string()))?;

    log::info!("Deleted stream {}", source_id);
    Ok(StatusCode::NO_CONTENT)
}

pub async fn wsc_rtp(
    State(state): State<Arc<GlobalState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(source_id): Path<u64>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Validate that the stream exists before upgrading to WebSocket
    if state.get_stream(source_id).await.is_err() {
        return (
            StatusCode::NOT_FOUND,
            format!("Stream {} not found", source_id),
        )
            .into_response();
    }

    ws.on_upgrade(move |socket| {
        handle_incoming_wsc_trp_websocket(socket, state, source_id, None, addr)
    })
}

#[utoipa::path(
    post,
    path = "/streams/{source_id}/webrtc",
    params(
        ("source_id" = u64, Path, description = "Stream unique identifier")
    ),
    request_body = CreateWebRtcSessionRequest,
    responses(
        (status = 201, description = "WebRTC session created successfully", body = CreateWebRtcSessionResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 404, description = "Stream not found", body = ErrorResponse)
    ),
    tag = "WebRTC"
)]
pub async fn create_webrtc_session(
    State(state): State<Arc<GlobalState>>,
    Path(source_id): Path<u64>,
    Json(req): Json<CreateWebRtcSessionRequest>,
) -> Result<(StatusCode, Json<CreateWebRtcSessionResponse>), ApiError> {
    log::debug!("Creating WebRTC session for stream {}", source_id);
    log::debug!("Received offer length: {}", req.offer.len());
    log::debug!(
        "Offer preview (first 200 chars): {}",
        req.offer.chars().take(200).collect::<String>()
    );

    let (session_id, answer) = state
        .create_websocket_session(req, source_id)
        .await
        .map_err(|e| {
            log::error!(
                "Failed to create WebRTC session for stream {}: {}",
                source_id,
                e
            );
            ApiError::BadRequest(e.to_string())
        })?;

    Ok((
        StatusCode::CREATED,
        Json(CreateWebRtcSessionResponse { session_id, answer }),
    ))
}

#[utoipa::path(
    delete,
    path = "/streams/{source_id}/webrtc/{session_id}",
    params(
        ("session_id" = Uuid, Path, description = "WebRTC session unique identifier")
    ),
    responses(
        (status = 204, description = "WebRTC session deleted successfully"),
        (status = 404, description = "Stream or session not found", body = ErrorResponse)
    ),
    tag = "WebRTC"
)]
pub async fn delete_webrtc_session(
    State(state): State<Arc<GlobalState>>,
    Path(session_id): Path<ClientSessionId>,
) -> Result<StatusCode, ApiError> {
    state
        .delete_webrtc_session(&session_id)
        .await
        .map_err(|e: anyhow::Error| ApiError::NotFound(e.to_string()))?;

    log::info!("Deleted WebRTC session {session_id}",);

    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/streams/{source_id}/webrtc/dvr",
    params(
        ("source_id" = u64, Path, description = "Stream unique identifier")
    ),
    request_body = CreateDvrWebRtcSessionRequest,
    responses(
        (status = 201, description = "WebRTC DVR session created successfully", body = CreateWebRtcSessionResponse),
        (status = 400, description = "Invalid request", body = ErrorResponse),
        (status = 404, description = "Stream not found", body = ErrorResponse)
    ),
    tag = "WebRTC"
)]
pub async fn create_dvr_webrtc_session(
    State(state): State<Arc<GlobalState>>,
    Path(source_id): Path<u64>,
    Json(req): Json<CreateDvrWebRtcSessionRequest>,
) -> Result<(StatusCode, Json<CreateWebRtcSessionResponse>), ApiError> {
    log::debug!(
        "Creating WebRTC DVR session for stream {} at timestamp {}",
        source_id,
        req.timestamp
    );

    // First create a live session
    let create_req = CreateWebRtcSessionRequest {
        offer: req.offer.clone(),
    };

    let (session_id, answer) = state
        .clone()
        .create_websocket_session(create_req, source_id)
        .await
        .map_err(|e| {
            log::error!(
                "Failed to create WebRTC DVR session for stream {}: {}",
                source_id,
                e
            );
            ApiError::BadRequest(e.to_string())
        })?;

    // Now seek to the requested timestamp
    state
        .seek_session(&session_id, req.timestamp)
        .await
        .map_err(|e| {
            log::error!(
                "Failed to seek DVR session {} to timestamp {}: {}",
                session_id,
                req.timestamp,
                e
            );
            ApiError::BadRequest(e.to_string())
        })?;

    Ok((
        StatusCode::CREATED,
        Json(CreateWebRtcSessionResponse { session_id, answer }),
    ))
}

#[utoipa::path(
    get,
    path = "/streams/{source_id}/webrtc",
    params(
        ("source_id" = u64, Path, description = "Stream unique identifier")
    ),
    responses(
        (status = 200, description = "List of WebRTC sessions for this stream", body = ListWebRtcSessionsResponse),
        (status = 404, description = "Stream not found", body = ErrorResponse)
    ),
    tag = "WebRTC"
)]
pub async fn list_webrtc_sessions(
    State(state): State<Arc<GlobalState>>,
    Path(source_id): Path<u64>,
) -> Result<Json<ListWebRtcSessionsResponse>, ApiError> {
    // Verify stream exists
    state
        .get_stream(source_id)
        .await
        .map_err(|_| ApiError::NotFound(format!("Stream {} not found", source_id)))?;

    let sessions = state.list_webrtc_sessions(source_id).await;

    Ok(Json(ListWebRtcSessionsResponse { sessions }))
}

#[utoipa::path(
    get,
    path = "/debug/webrtc-client",
    responses(
        (status = 200, description = "WebRTC test client HTML page")
    ),
    tag = "Debug"
)]
pub async fn webrtc_client() -> Html<&'static str> {
    Html(include_str!("webrtc-client.html"))
}

#[utoipa::path(
    post,
    path = "/streams/{source_id}/webrtc/{session_id}/seek",
    params(
        ("source_id" = u64, Path, description = "Stream unique identifier"),
        ("session_id" = Uuid, Path, description = "WebRTC session unique identifier")
    ),
    request_body = SeekRequest,
    responses(
        (status = 200, description = "Seek successful", body = SessionModeResponse),
        (status = 404, description = "Stream or session not found", body = ErrorResponse),
        (status = 400, description = "No recording available for timestamp", body = ErrorResponse)
    ),
    tag = "WebRTC"
)]
pub async fn seek_session(
    State(state): State<Arc<GlobalState>>,
    Path((source_id, session_id)): Path<(u64, Uuid)>,
    Json(req): Json<SeekRequest>,
) -> Result<Json<SessionModeResponse>, ApiError> {
    log::debug!("seeking session {session_id} of stream {source_id} to {req:?}");
    state
        .seek_webrtc_session(source_id, session_id, req.timestamp)
        .await
        .map_err(|e| {
            log::error!(
                "Failed to seek session {} to timestamp {}: {}",
                session_id,
                req.timestamp,
                e
            );
            ApiError::BadRequest(e.to_string())
        })?;

    let mode = state
        .get_session_mode(source_id, session_id)
        .await
        .map_err(|e| ApiError::NotFound(e.to_string()))?;

    log::info!(
        "Session {} seeked to timestamp {}",
        session_id,
        req.timestamp
    );

    Ok(Json(mode))
}

#[utoipa::path(
    post,
    path = "/streams/{source_id}/webrtc/{session_id}/speed",
    params(
        ("source_id" = u64, Path, description = "Stream unique identifier"),
        ("session_id" = Uuid, Path, description = "WebRTC session unique identifier")
    ),
    request_body = SpeedRequest,
    responses(
        (status = 200, description = "Speed changed successfully", body = SessionModeResponse),
        (status = 404, description = "Stream or session not found", body = ErrorResponse),
        (status = 400, description = "Speed control only available in DVR mode", body = ErrorResponse)
    ),
    tag = "WebRTC"
)]
pub async fn set_session_speed(
    State(state): State<Arc<GlobalState>>,
    Path((source_id, session_id)): Path<(u64, Uuid)>,
    Json(req): Json<SpeedRequest>,
) -> Result<Json<SessionModeResponse>, ApiError> {
    state
        .set_session_speed(source_id, session_id, req.speed)
        .await
        .map_err(|e| {
            log::error!("Failed to set speed for session {}: {}", session_id, e);
            ApiError::BadRequest(e.to_string())
        })?;

    let mode = state
        .get_session_mode(source_id, session_id)
        .await
        .map_err(|e| ApiError::NotFound(e.to_string()))?;

    log::info!("Session {} speed changed to {}x", session_id, req.speed);

    Ok(Json(mode))
}

#[utoipa::path(
    post,
    path = "/streams/{source_id}/webrtc/{session_id}/live",
    params(
        ("source_id" = u64, Path, description = "Stream unique identifier"),
        ("session_id" = Uuid, Path, description = "WebRTC session unique identifier")
    ),
    responses(
        (status = 200, description = "Switched to live mode", body = SessionModeResponse),
        (status = 404, description = "Stream or session not found", body = ErrorResponse)
    ),
    tag = "WebRTC"
)]
pub async fn switch_session_to_live(
    State(state): State<Arc<GlobalState>>,
    Path((source_id, client_session_id)): Path<(u64, Uuid)>,
) -> Result<Json<SessionModeResponse>, ApiError> {
    state
        .switch_session_to_live(&client_session_id)
        .await
        .map_err(|e| {
            log::error!(
                "Failed to switch session {} to live: {}",
                client_session_id,
                e
            );
            ApiError::BadRequest(e.to_string())
        })?;

    let mode = state
        .get_session_mode(source_id, client_session_id)
        .await
        .map_err(|e| ApiError::NotFound(e.to_string()))?;

    log::info!("Session {} switched to live mode", client_session_id);

    Ok(Json(mode))
}

#[utoipa::path(
    get,
    path = "/streams/{source_id}/webrtc/{session_id}/mode",
    params(
        ("source_id" = u64, Path, description = "Stream unique identifier"),
        ("session_id" = Uuid, Path, description = "WebRTC session unique identifier")
    ),
    responses(
        (status = 200, description = "Current session mode", body = SessionModeResponse),
        (status = 404, description = "Stream or session not found", body = ErrorResponse)
    ),
    tag = "WebRTC"
)]
pub async fn get_session_mode(
    State(state): State<Arc<GlobalState>>,
    Path((source_id, session_id)): Path<(u64, Uuid)>,
) -> Result<Json<SessionModeResponse>, ApiError> {
    let mode = state
        .get_session_mode(source_id, session_id)
        .await
        .map_err(|e| ApiError::NotFound(e.to_string()))?;

    Ok(Json(mode))
}
