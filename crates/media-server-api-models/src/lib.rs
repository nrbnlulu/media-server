use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateStreamRequest {
    pub source_id: u64,
    pub rtsp_url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub should_record: bool,
    pub restart_interval_secs: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateStreamResponse {
    pub source_id: u64,
    pub rtsp_url: String,
    pub should_record: bool,
    pub restart_interval_secs: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StreamResponse {
    pub source_id: u64,
    pub rtsp_url: String,
    pub state: String,
    pub should_record: bool,
    pub webrtc_sessions: Vec<Uuid>,
    pub restart_interval_secs: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ListStreamsResponse {
    pub streams: Vec<StreamResponse>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateWebRtcSessionRequest {
    pub offer: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateDvrWebRtcSessionRequest {
    pub offer: String,
    pub timestamp: u64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateWebRtcSessionResponse {
    pub session_id: Uuid,
    pub answer: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ErrorResponse {
    pub error: String,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SeekRequest {
    pub timestamp: u64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SpeedRequest {
    pub speed: f64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SessionModeResponse {
    pub is_live: bool,
    pub current_time_ms: Option<u64>,
    pub speed: f64,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct WebRtcSessionResponse {
    pub session_id: Uuid,
    pub source_id: u64,
    pub is_live: bool,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ListWebRtcSessionsResponse {
    pub sessions: Vec<WebRtcSessionResponse>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WscRtpClientMessage {
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Live,
    Dvr { timestamp: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WscRtpServerMessage {
    Init {
        token: String,
        server_port: u16,
        udp_holepunch_required: bool,
    },
    Sdp {
        sdp: String,
    },
    StreamState {
        state: String,
    },
    SessionMode(SessionMode),
    Error {
        message: String,
    },
    Pong,
}
