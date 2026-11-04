use crate::common::rtp::{RtpPacketizer, StitchingConsumer};
use crate::common::traits::{RtpConsumer, RtpVideoPublisher, VideoSource};
use crate::domain::dvr::{filesystem, recorder::DvrRecorder};
use crate::domain::{StreamConfig, StreamInfo, StreamState};
use crate::publishers::webrtc::WebrtcManager;
use crate::publishers::wsc_rtp::WscRtpPublisher;
use crate::sources::rtsp::RtspClient;
use crate::sources::segmented_dvr::SegmentedPlayer as DvrPlayer;
use anyhow::{Result, anyhow, bail};
use dashmap::DashMap;
use gstreamer::glib::clone::Downgrade;
use media_server_api_models::UnixTimestamp;
use media_server_api_models::{ClientSessionId, CreateWebRtcSessionRequest, SessionMode};
use serde::Deserialize;
use std::sync::{Arc, Weak};
use tokio::sync::broadcast;

struct SourceWrapper {
    source: Arc<dyn VideoSource>,
    rtsp_client: Arc<RtspClient>,
    task: tokio::task::JoinHandle<()>,
    rtp_packetizer: Arc<RtpPacketizer>,
    /// Held across recorder startup so concurrent enable requests serialize.
    recorder: Arc<tokio::sync::Mutex<Option<Arc<DvrRecorder>>>>,
}

pub type VideoSourceId = String;
pub type SharedGlobalState = Arc<GlobalState>;
pub type WeakGlobalState = Weak<GlobalState>;

#[derive(Deserialize, Debug)]
pub struct AppSettings {}

impl AppSettings {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {})
    }
}
#[derive(Clone)]
pub enum ClientSessionState {
    Live,
    Dvr(Arc<DvrPlayer>),
}

#[allow(dead_code)]
struct ClientSession {
    id: ClientSessionId,
    source_id: VideoSourceId,
    publisher: Arc<dyn RtpVideoPublisher>,
    live_packetizer: Arc<RtpPacketizer>,
    stitching_consumer: Arc<StitchingConsumer>,
    state: tokio::sync::Mutex<ClientSessionState>,
}

impl ClientSession {
    pub fn source_id(&self) -> &VideoSourceId {
        &self.source_id
    }

    pub fn new_live(
        id: ClientSessionId,
        source_id: VideoSourceId,
        publisher: Arc<dyn RtpVideoPublisher>,
        live_packetizer: Arc<RtpPacketizer>,
    ) -> Self {
        let stitching_consumer = Arc::new(StitchingConsumer::new(publisher.clone()));
        Self {
            id,
            source_id,
            publisher,
            stitching_consumer,
            live_packetizer,
            state: tokio::sync::Mutex::new(ClientSessionState::Live),
        }
    }

    pub async fn switch_to_live(&self) {
        let mut state = self.state.lock().await;
        if let ClientSessionState::Dvr(player) = &*state {
            if let Err(error) = player.terminate().await {
                log::error!("Failed to stop DVR: {error:#}");
            }
            let input = self.stitching_consumer.next_input().await;
            // Queue the new mode before making any new live frames eligible.
            self.publisher
                .on_session_mode_change(SessionMode::Live)
                .await;
            self.live_packetizer.add_consumer(input);
            *state = ClientSessionState::Live;
        }
    }

    pub async fn seek(&self, timestamp: UnixTimestamp) -> Result<()> {
        let mut state_guard = self.state.lock().await;
        match *state_guard {
            ClientSessionState::Dvr(ref player) => {
                player.seek_to_timestamp(timestamp, 1.0).await?;
                self.publisher
                    .on_session_mode_change(SessionMode::Dvr {
                        timestamp: player.current_timestamp().await,
                    })
                    .await;
                Ok(())
            }
            ClientSessionState::Live => {
                // Validate timestamp before switching mode — don't enter DVR if no recording exists
                if !DvrPlayer::available(&self.source_id, timestamp) {
                    bail!("No recording found for timestamp {}", timestamp);
                }
                // Create DVR player that feeds into the same stitching consumer
                self.live_packetizer
                    .remove_consumer(self.stitching_consumer.as_ref());
                let input = self.stitching_consumer.next_input().await;
                let player = match DvrPlayer::new(self.source_id.clone(), timestamp, input).await {
                    Ok(player) => Arc::new(player),
                    Err(error) => {
                        self.live_packetizer
                            .add_consumer(self.stitching_consumer.next_input().await);
                        return Err(error);
                    }
                };

                let player_clone = player.clone();
                match player.play().await {
                    Ok(()) => {
                        *state_guard = ClientSessionState::Dvr(player.clone());

                        // Notify the client of the mode change
                        self.publisher
                            .on_session_mode_change(SessionMode::Dvr {
                                timestamp: player.current_timestamp().await,
                            })
                            .await;
                        Ok(())
                    }
                    Err(e) => {
                        let _ = player_clone.terminate().await;
                        // Re-add so live stream is restored
                        self.live_packetizer
                            .add_consumer(self.stitching_consumer.next_input().await);
                        Err(e)
                    }
                }
            }
        }
    }

    async fn terminate(&self) {
        let state = self.state.lock().await;
        match *state {
            ClientSessionState::Live => {
                // not much to do
            }
            ClientSessionState::Dvr(ref player) => {
                if let Err(e) = player.terminate().await {
                    log::error!("Failed to terminate DVR player: {}", e);
                }
            }
        }
    }
}
pub struct GlobalState {
    sources: DashMap<VideoSourceId, SourceWrapper>,
    client_sessions: DashMap<ClientSessionId, ClientSession>,
    wsc_publishers: DashMap<ClientSessionId, Arc<WscRtpPublisher>>,
    extra_shutdown_tasks: Vec<tokio::task::JoinHandle<()>>,
    webrtc_manager: WebrtcManager,
    settings: AppSettings,
}

impl GlobalState {
    pub async fn new() -> anyhow::Result<Self> {
        let settings = AppSettings::new()?;
        let webrtc_manager = WebrtcManager::new()?;

        let res = Self {
            sources: DashMap::new(),
            client_sessions: DashMap::new(),
            wsc_publishers: DashMap::new(),
            extra_shutdown_tasks: Vec::new(),
            webrtc_manager,
            settings,
        };
        Ok(res)
    }

    pub fn settings(&self) -> &AppSettings {
        &self.settings
    }

    pub async fn create_rtsp_stream(&self, config: StreamConfig) -> Result<VideoSourceId> {
        let source_id = config.source_id.clone();
        if source_id.starts_with("prealarm:") {
            bail!("The prealarm: source prefix is reserved for authenticated imports");
        }

        if self.sources.contains_key(&source_id) {
            bail!("Stream {} already exists", source_id);
        }

        for entry in self.sources.iter() {
            let existing_inputs = entry.value().source.inputs();
            for existing_input in existing_inputs {
                for new_input in &config.rtsp_inputs {
                    if existing_input.url == new_input.url {
                        bail!(
                            "RTSP URL {} already exists with source id {}",
                            existing_input.url,
                            entry.value().source.source_id()
                        );
                    }
                }
            }
        }
        let (state_tx, _) = broadcast::channel(100);
        let rtp_packetizer = Arc::new(RtpPacketizer::new(rand::random(), 96));
        let rtsp_client = Arc::new(RtspClient::new(
            config.clone(),
            state_tx.clone(),
            // by default we add a rtp packetizer consumer since we need it both in webrtc and wsc-rtp
            vec![rtp_packetizer.clone()],
        )?);

        let mut recorder = None;
        if config.should_record
            && let Some(started) = Self::start_recorder(&source_id, &rtsp_client).await
        {
            recorder = Some(started);
        }
        let rtsp_client_clone = rtsp_client.clone();
        let handle = tokio::spawn(async move {
            rtsp_client_clone.execute().await;
        });

        self.sources.insert(
            source_id.clone(),
            SourceWrapper {
                source: rtsp_client.clone(),
                rtsp_client,
                task: handle,
                rtp_packetizer,
                recorder: Arc::new(tokio::sync::Mutex::new(recorder)),
            },
        );

        log::info!("Stream {} created and started", source_id);
        Ok(source_id)
    }

    async fn start_recorder(
        source_id: &VideoSourceId,
        rtsp_client: &RtspClient,
    ) -> Option<Arc<DvrRecorder>> {
        let result = async {
            filesystem::ensure_stream_dvr_dir(source_id)?;
            let recorder = Arc::new(DvrRecorder::new(source_id)?);
            rtsp_client.add_consumer(recorder.clone()).await?;
            anyhow::Ok(recorder)
        }
        .await;
        match result {
            Ok(recorder) => Some(recorder),
            Err(e) => {
                log::error!("Failed to create recorder for stream {}: {}", source_id, e);
                None
            }
        }
    }

    /// Enables DVR recording on a running stream. Idempotent.
    pub async fn start_recording(&self, source_id: &VideoSourceId) -> Result<()> {
        // Clone out of the map so no DashMap guard is held across awaits.
        let (rtsp_client, recorder_slot) = {
            let source = self
                .sources
                .get(source_id)
                .ok_or_else(|| anyhow!("Stream {} not found", source_id))?;
            (source.rtsp_client.clone(), source.recorder.clone())
        };
        let mut slot = recorder_slot.lock().await;
        if slot.is_some() {
            return Ok(());
        }
        let recorder = Self::start_recorder(source_id, &rtsp_client)
            .await
            .ok_or_else(|| anyhow!("Failed to start recording for stream {}", source_id))?;
        *slot = Some(recorder);
        Ok(())
    }

    pub async fn delete_stream(&self, source_id: &VideoSourceId) -> Result<()> {
        if let Some((_, src)) = self.sources.remove(source_id) {
            let _ = src.source.stop().await;
            src.task.abort();

            log::info!("Stream {} and all its sessions ", source_id);
        }
        Ok(())
    }

    pub async fn get_stream(&self, source_id: &VideoSourceId) -> Result<StreamInfo> {
        let source = self
            .sources
            .get(source_id)
            .ok_or_else(|| anyhow!("Stream {} not found", source_id))?;

        let state = source.source.state().await;

        let recorder = source.recorder.lock().await;
        let recording_start_time = recorder.as_ref().and_then(|r| r.start_time());

        Ok(StreamInfo {
            source_id: source_id.clone(),
            rtsp_inputs: source.source.inputs().to_vec(),
            state,
            should_record: recorder.is_some(),
            restart_interval_secs: source.source.config().restart_interval_secs,
            recording_start_time,
        })
    }

    pub async fn list_streams(&self) -> Vec<StreamInfo> {
        let mut streams = Vec::new();
        for entry in self.sources.iter() {
            if let Ok(info) = self.get_stream(entry.key()).await {
                streams.push(info);
            }
        }
        streams
    }

    pub async fn get_source(&self, source_id: &VideoSourceId) -> Result<Arc<dyn VideoSource>> {
        let source = self
            .sources
            .get(source_id)
            .ok_or_else(|| anyhow!("Stream {} not found", source_id))?;

        Ok(source.source.clone())
    }

    pub async fn subscribe_stream_state(
        &self,
        source_id: &VideoSourceId,
    ) -> Result<broadcast::Receiver<StreamState>> {
        let source = self
            .sources
            .get(source_id)
            .ok_or_else(|| anyhow!("Stream {} not found", source_id))?;

        Ok(source.source.subscribe_state())
    }

    pub async fn create_websocket_session(
        self: Arc<Self>,
        request: CreateWebRtcSessionRequest,
        source_id: VideoSourceId,
    ) -> Result<(ClientSessionId, String)> {
        self.create_rtc_session(request, source_id, None).await
    }
    pub async fn create_dvr_webrtc_session(
        self: Arc<Self>,
        request: CreateWebRtcSessionRequest,
        source_id: VideoSourceId,
        timestamp: u64,
    ) -> Result<(ClientSessionId, String)> {
        self.create_rtc_session(request, source_id, Some(timestamp))
            .await
    }
    async fn create_rtc_session(
        self: Arc<Self>,
        request: CreateWebRtcSessionRequest,
        source_id: VideoSourceId,
        timestamp: Option<u64>,
    ) -> Result<(ClientSessionId, String)> {
        let packetizer = self
            .sources
            .get(&source_id)
            .map(|source| source.rtp_packetizer.clone())
            .ok_or_else(|| anyhow!("Stream not found"))?;
        let params = if let Some(timestamp) = timestamp {
            DvrPlayer::codec_parameters(&source_id, timestamp).await?
        } else {
            packetizer
                .get_codec_params()
                .ok_or_else(|| anyhow!("Codec parameters not found - stream may not be ready"))?
        };
        let (publisher, answer) = self
            .webrtc_manager
            .create_new_session(source_id.clone(), params, request, self.downgrade())
            .await?;
        let id = *publisher.id();
        let session = ClientSession::new_live(id, source_id, publisher, packetizer.clone());
        if let Some(timestamp) = timestamp {
            if let Err(error) = session.seek(timestamp).await {
                let _ = self.webrtc_manager.delete_session(&id).await;
                return Err(error);
            }
        } else {
            packetizer.add_consumer(session.stitching_consumer.input());
        }
        self.client_sessions.insert(id, session);
        Ok((id, answer))
    }

    pub async fn delete_webrtc_session(&self, session_id: &ClientSessionId) -> anyhow::Result<()> {
        self.delete_client_session(session_id).await?;
        self.webrtc_manager.delete_session(session_id).await
    }

    pub async fn create_wsc_rtp_registration(
        &self,
        source_id: VideoSourceId,
    ) -> Result<Arc<WscRtpPublisher>> {
        if let Some(source) = self.sources.get(&source_id).as_ref() {
            let packetizer = &source.rtp_packetizer;

            // Get the source inputs to pass to the publisher
            let publisher = Arc::new(WscRtpPublisher::new(source_id.clone()));
            let client_id = *publisher.id();

            // Store the publisher for later retrieval
            self.wsc_publishers.insert(client_id, publisher.clone());

            // Create and store the client session
            let client_session = ClientSession::new_live(
                client_id,
                source_id.clone(),
                publisher.clone(),
                packetizer.clone(),
            );
            let stitching_consumer = client_session.stitching_consumer.input();
            self.client_sessions.insert(client_id, client_session);

            // Add the stitching consumer (which wraps the publisher) to the packetizer
            packetizer.add_consumer(stitching_consumer);
            Ok(publisher)
        } else {
            bail!("Stream not found")
        }
    }

    pub async fn delete_client_session(&self, session_id: &ClientSessionId) -> anyhow::Result<()> {
        if let Some((_id, session)) = self.client_sessions.remove(session_id) {
            if let Some(ref source) = self.sources.get(session.source_id()) {
                source
                    .rtp_packetizer
                    .remove_consumer(session.stitching_consumer.as_ref());
            } else {
                log::error!("Stream not found when deleting session {}", session_id);
            }
            session.terminate().await;
        } else {
            bail!("session not found");
        }
        Ok(())
    }

    pub async fn delete_wsc_rtp_session(&self, id: &ClientSessionId) -> Result<()> {
        self.delete_client_session(id).await?;
        if let Some((_, publisher)) = self.wsc_publishers.remove(id) {
            publisher.shutdown();
        }
        Ok(())
    }

    pub async fn get_client_session_state(
        &self,
        id: &ClientSessionId,
    ) -> Result<ClientSessionState> {
        if let Some(ref session) = self.client_sessions.get(id) {
            return Ok(session.state.lock().await.clone());
        }
        bail!("Session not found");
    }

    pub async fn switch_session_to_live(&self, session_id: &ClientSessionId) -> Result<()> {
        if let Some(ref session) = self.client_sessions.get(session_id) {
            session.switch_to_live().await;
            return Ok(());
        }
        bail!("Session not found");
    }

    pub async fn seek_session(
        &self,
        session_id: &ClientSessionId,
        timestamp: UnixTimestamp,
    ) -> Result<()> {
        if let Some(ref session) = self.client_sessions.get(session_id) {
            if self.sources.contains_key(session.source_id()) {
                return session.seek(timestamp).await;
            }
            bail!("Source not found");
        }
        bail!("Session not found");
    }

    pub async fn get_session_mode(
        &self,
        session_id: ClientSessionId,
    ) -> Result<media_server_api_models::SessionModeResponse> {
        let session_state = self.get_client_session_state(&session_id).await?;
        match session_state {
            ClientSessionState::Live => Ok(media_server_api_models::SessionModeResponse {
                is_live: true,
                current_time_ms: None,
                speed: 1.0,
                playback: self
                    .client_sessions
                    .get(&session_id)
                    .and_then(|session| session.publisher.playback_event()),
            }),
            ClientSessionState::Dvr(player) => {
                let current_time = Some(player.current_timestamp().await);
                Ok(media_server_api_models::SessionModeResponse {
                    is_live: false,
                    current_time_ms: current_time,
                    speed: player.speed(),
                    playback: Some(player.playback_event()),
                })
            }
        }
    }

    pub async fn set_session_speed(&self, session_id: ClientSessionId, speed: f64) -> Result<()> {
        if let Some(ref session) = self.client_sessions.get(&session_id) {
            let state_guard = session.state.lock().await;
            if let ClientSessionState::Dvr(ref player) = *state_guard {
                player.set_speed(speed)?;
                return Ok(());
            }
            bail!("Speed control only available in DVR mode");
        }
        bail!("Session not found");
    }

    pub async fn list_webrtc_sessions(
        &self,
        source_id: &VideoSourceId,
    ) -> Vec<media_server_api_models::WebRtcSessionResponse> {
        let mut sessions = Vec::new();
        for entry in self.client_sessions.iter() {
            if entry.value().source_id() == source_id {
                let state = entry.value().state.lock().await;
                let is_live = matches!(*state, ClientSessionState::Live);
                sessions.push(media_server_api_models::WebRtcSessionResponse {
                    session_id: *entry.key(),
                    source_id: source_id.clone(),
                    is_live,
                });
            }
        }
        sessions
    }

    pub async fn cleanup(&self) -> Result<()> {
        log::info!("Starting cleanup of all streams...");

        let session_ids: Vec<ClientSessionId> = self
            .client_sessions
            .iter()
            .map(|entry| *entry.key())
            .collect();
        for session_id in session_ids {
            if self.wsc_publishers.contains_key(&session_id) {
                let _ = self.delete_wsc_rtp_session(&session_id).await;
            } else {
                let _ = self.delete_webrtc_session(&session_id).await;
            }
        }

        let source_ids: Vec<VideoSourceId> = self
            .sources
            .iter()
            .map(|entry| entry.key().clone())
            .collect();

        for source_id in source_ids {
            log::info!("Stopping stream {} during cleanup", source_id);
            if let Err(e) = self.delete_stream(&source_id).await {
                log::error!("Error stopping stream {} during cleanup: {}", source_id, e);
            }
        }
        for task in &self.extra_shutdown_tasks {
            task.abort();
        }

        log::info!("Cleanup complete");
        Ok(())
    }
}

#[cfg(test)]
mod import_namespace_tests {
    use super::*;
    #[tokio::test]
    async fn live_registration_cannot_expose_an_imported_source() {
        let state = GlobalState::new().await.unwrap();
        let result = state
            .create_rtsp_stream(StreamConfig {
                source_id: "prealarm:incident:source".into(),
                rtsp_inputs: vec![],
                username: None,
                password: None,
                should_record: true,
                restart_interval_secs: None,
            })
            .await;
        assert!(result.unwrap_err().to_string().contains("reserved"));
        assert!(state.list_streams().await.is_empty());
    }
}
