use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

pub type UnixTimestamp = u64;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct VideoSourceInput {
    pub label: String,
    #[schema(value_type = String, format = "uri")]
    pub url: url::Url,
    /// Priority of the input source. Lower values have higher priority.
    pub priority: u32,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateStreamRequest {
    pub source_id: String,
    pub rtsp_inputs: Vec<VideoSourceInput>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub should_record: bool,
    pub restart_interval_secs: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct CreateStreamResponse {
    pub source_id: String,
    pub rtsp_inputs: Vec<VideoSourceInput>,
    pub should_record: bool,
    pub restart_interval_secs: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StreamResponse {
    pub source_id: String,
    pub rtsp_inputs: Vec<VideoSourceInput>,
    pub state: String,
    pub should_record: bool,
    pub restart_interval_secs: Option<u64>,
    pub recording_start_time: Option<UnixTimestamp>,
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
    pub source_id: String,
    pub is_live: bool,
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ListWebRtcSessionsResponse {
    pub sessions: Vec<WebRtcSessionResponse>,
}

/// Query parameters for WSC-RTP WebSocket endpoint
#[derive(Debug, Serialize, Deserialize)]
pub struct WscRtpQuery {
    /// Skip UDP negotiation and use WebSocket for RTP delivery from the start
    #[serde(default)]
    pub force_websocket_transport: bool,
}

pub mod wsc_rtp {
    /// Header for UDP hole-punch packets sent by the client
    pub const HOLEPUNCH_HEADER: &str = "ws-rtp";

    /// Header for dummy packets sent by the server to confirm UDP connectivity
    pub const DUMMY_HEADER: &str = "ws-rtp-dummy";

    /// Header for ack packets sent by the client to confirm UDP receipt
    pub const ACK_HEADER: &str = "ws-rtp-ack";

    /// Number of dummy UDP packets to send while waiting for client ack
    pub const DUMMY_PACKET_COUNT: u32 = 5;

    /// Interval between dummy packets in milliseconds
    pub const DUMMY_PACKET_INTERVAL_MS: u64 = 100;

    /// Port range start for UDP holepunch listeners
    pub const PORT_RANGE_START: u16 = 30000;

    /// Port range end for UDP holepunch listeners
    pub const PORT_RANGE_END: u16 = 40000;
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
        session_id: String,
        /// The port allocated for this session's UDP hole punch listener (30000-40000 range)
        holepunch_port: u16,
        /// the elected stream source for this session
        active_source: Option<VideoSourceInput>,
    },
    Sdp {
        sdp: String,
    },
    SessionMode(SessionMode),
    Error {
        message: String,
    },
    /// Sent when UDP transport fails and RTP packets will now be delivered as binary WebSocket frames.
    FallingBackRtpToWs,
    Pong,
}
