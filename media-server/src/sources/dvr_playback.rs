//! Select segmented recordings or the legacy single-file playback implementation.
use super::{dvr::DvrPlayer, segmented_dvr::SegmentedPlayer};
use crate::{
    app::VideoSourceId,
    common::{VideoCodec, traits::RtpConsumer},
    domain::dvr::{
        filesystem,
        segments::{self, Session},
    },
};
use anyhow::Result;
use std::sync::Arc;

enum Backend {
    Legacy(DvrPlayer),
    Segmented(SegmentedPlayer),
}
pub struct DvrPlayback {
    source: VideoSourceId,
    codec: VideoCodec,
    consumer: Arc<dyn RtpConsumer>,
    backend: tokio::sync::Mutex<Backend>,
}
fn segmented(source: &VideoSourceId, timestamp: u64) -> Result<bool> {
    Ok(
        Session::active(source).is_some_and(|s| s.contains(timestamp))
            || segments::list(source)?
                .iter()
                .any(|s| s.start_ms() <= timestamp && timestamp <= s.end_ms()),
    )
}
impl DvrPlayback {
    pub async fn new(
        source: VideoSourceId,
        timestamp: u64,
        codec: VideoCodec,
        consumer: Arc<dyn RtpConsumer>,
    ) -> Result<Self> {
        let backend = Self::open(&source, timestamp, codec, consumer.clone()).await?;
        Ok(Self {
            source,
            codec,
            consumer,
            backend: tokio::sync::Mutex::new(backend),
        })
    }
    async fn open(
        source: &VideoSourceId,
        timestamp: u64,
        codec: VideoCodec,
        consumer: Arc<dyn RtpConsumer>,
    ) -> Result<Backend> {
        if segmented(source, timestamp)? {
            Ok(Backend::Segmented(
                SegmentedPlayer::new(source.clone(), timestamp, consumer).await?,
            ))
        } else {
            Ok(Backend::Legacy(
                DvrPlayer::new(source.clone(), timestamp, codec, consumer).await?,
            ))
        }
    }
    pub fn available(source: &VideoSourceId, timestamp: u64) -> bool {
        segmented(source, timestamp).unwrap_or(false)
            || filesystem::find_recording_for_timestamp(source, timestamp).is_some()
    }
    pub fn speed(&self) -> f64 {
        1.0
    }
    pub fn set_speed(&self, _speed: f64) {
        log::warn!("set_speed not fully implemented yet");
    }
    pub async fn play(&self) -> Result<()> {
        match &*self.backend.lock().await {
            Backend::Legacy(p) => p.play().await,
            Backend::Segmented(p) => p.play().await,
        }
    }
    pub async fn terminate(&self) -> Result<()> {
        match &*self.backend.lock().await {
            Backend::Legacy(p) => p.terminate().await,
            Backend::Segmented(p) => p.terminate().await,
        }
    }
    pub async fn current_timestamp(&self) -> u64 {
        match &*self.backend.lock().await {
            Backend::Legacy(p) => p.current_timestamp().await,
            Backend::Segmented(p) => p.current_timestamp().await,
        }
    }
    pub async fn seek_to_timestamp(&self, timestamp: u64, speed: f64) -> Result<()> {
        let mut backend = self.backend.lock().await;
        let target_segmented = segmented(&self.source, timestamp)?;
        match &*backend {
            Backend::Segmented(p) if target_segmented => {
                return p.seek_to_timestamp(timestamp, speed).await;
            }
            Backend::Legacy(p) if !target_segmented => {
                let metadata = p.recording_metadata().await;
                if metadata.contains(timestamp) {
                    return Ok(p.seek_to_timestamp(timestamp, speed).await?);
                }
            }
            _ => {}
        }
        let replacement =
            Self::open(&self.source, timestamp, self.codec, self.consumer.clone()).await?;
        // Prepare the replacement before interrupting the current playback.
        if let Backend::Legacy(p) = &replacement {
            p.prepare().await?;
        }
        match &*backend {
            Backend::Legacy(p) => p.terminate().await?,
            Backend::Segmented(p) => p.terminate().await?,
        }
        match &replacement {
            Backend::Legacy(p) => p.play().await?,
            Backend::Segmented(p) => p.play().await?,
        }
        *backend = replacement;
        Ok(())
    }
}
