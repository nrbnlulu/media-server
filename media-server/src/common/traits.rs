use crate::{
    app::{ClientSessionId, VideoSourceId},
    common::{rtp::RtpPacket, FFmpegVideoMetadata, VideoCodec},
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}
use crate::domain::{StreamConfig, StreamState};

/// Trait for video sources (RTSP, DVR, file playback, etc.)
#[async_trait]
pub trait VideoSource: Send + Sync {
    async fn execute(&self);
    async fn codec(&self) -> Option<VideoCodec>;
    async fn stop(&self) -> anyhow::Result<()>;
    fn url(&self) -> &url::Url;
    fn source_id(&self) -> &VideoSourceId;
    async fn state(&self) -> StreamState;
    fn config(&self) -> &StreamConfig;
    fn subscribe_state(&self) -> broadcast::Receiver<StreamState>;
}

#[async_trait]
pub trait FfmpegConsumer: Send + Sync {
    fn initialize(&self, metadata: &FFmpegVideoMetadata) -> anyhow::Result<()>;
    async fn on_new_packet(&self, packet: Arc<ffmpeg::Packet>) -> anyhow::Result<()>;
    async fn finalize(&self) -> anyhow::Result<()>;
}

#[async_trait]
pub trait RtpConsumer: Send + Sync {
    fn id(&self) -> &ClientSessionId;
    /// Called for each RTP frame. The frame's payload is shared (Arc) across consumers.
    /// Implementations should serialize with `frame.to_bytes()` or access header/payload separately.
    async fn on_new_packet(&self, frame: Arc<RtpPacket>);
    async fn finalize(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

pub trait RtpVideoPublisher: RtpConsumer {
    fn source_id(&self) -> &VideoSourceId;
}

impl PartialEq for dyn RtpConsumer {
    fn eq(&self, other: &Self) -> bool {
        self.id() == other.id()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("Connection closed")]
    ConnectionClosed,

    #[error("Send buffer full")]
    BufferFull,

    #[error("IO error: {0}")]
    Io(String),

    #[error("Codec mismatch: expected {expected:?}, got {got:?}")]
    CodecMismatch {
        expected: VideoCodec,
        got: VideoCodec,
    },

    #[error("Not initialized")]
    NotInitialized,

    #[error("Packet too large: {size} > {max}")]
    PacketTooLarge { size: usize, max: usize },
}

// Re-export the codec parameters from the rtp module
pub use crate::common::rtp::CodecParameters;
