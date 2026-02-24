use crate::common::VideoCodec;
use crate::common::rtp::{RtpPacketizer, StitchingConsumer};
use crate::common::traits::{RtpConsumer, RtpVideoPublisher, VideoSource};
use crate::domain::dvr::{filesystem, recorder::DvrRecorder};
use crate::domain::{StreamConfig, StreamInfo, StreamState};
use crate::publishers::webrtc::WebrtcManager;
use crate::publishers::wsc_rtp::WscRtpPublisher;
use crate::sources::dvr::DvrPlayer;
use crate::sources::rtsp::RtspClient;
use anyhow::{Result, anyhow, bail};
use dashmap::DashMap;
use gstreamer::glib::clone::Downgrade;
use media_server_api_models::UnixTimestamp;
use media_server_api_models::{ClientSessionId, CreateWebRtcSessionRequest};
use serde::Deserialize;
use std::sync::{Arc, Weak};
use tokio::sync::broadcast;

struct SourceWrapper {
    source: Arc<dyn VideoSource>,
    task: tokio::task::JoinHandle<()>,
    rtp_packetizer: Arc<RtpPacketizer>,
    recorder: Option<Arc<DvrRecorder>>,
}

pub type VideoSourceId = String;
pub type SharedGlobalState = Arc<GlobalState>;
pub type WeakGlobalState = Weak<GlobalState>;

#[derive(Deserialize, Debug)]
pub struct AppSettings {}

impl AppSettings {
    pub fn new() -> anyhow::Result<Self> {
        envy::from_env::<Self>().map_err(|e| anyhow::anyhow!(e))
    }
}
#[derive(Clone)]
pub enum ClientSessionState {
    Live,
    Dvr(Arc<DvrPlayer>, Option<Arc<tokio::task::JoinHandle<()>>>),
}

struct ClientSession {
    id: ClientSessionId,
    source_id: VideoSourceId,
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
        let stitching_consumer = Arc::new(StitchingConsumer::new(publisher));
        Self {
            id,
            source_id,
            stitching_consumer,
            live_packetizer,
            state: tokio::sync::Mutex::new(ClientSessionState::Live),
        }
    }

    pub async fn switch_to_live(&self) {
        let mut state_guard = self.state.lock().await;
        match &*state_guard {
            ClientSessionState::Dvr(player, handle_opt) => {
                if let Err(err) = player.terminate().await {
                    log::error!("Failed to stop pipeline: {}", err);
                }

                // Abort the spawned task if it exists
                if let Some(handle) = handle_opt {
                    handle.abort();
                }

                // Re-add stitching consumer to live packetizer
                // Note: stitching consumer maintains seq continuity automatically
                self.live_packetizer
                    .add_consumer(self.stitching_consumer.clone());

                *state_guard = ClientSessionState::Live;
            }
            ClientSessionState::Live => {}
        }
    }

    pub async fn seek(&self, timestamp: UnixTimestamp, codec: &VideoCodec) -> Result<()> {
        let mut state_guard = self.state.lock().await;
        // first check that we're on dvr mode otherwise disable the live packetizer
        match *state_guard {
            ClientSessionState::Dvr(ref player, _) => {
                player.seek_to_timestamp(timestamp, 1.0).await?;
            }
            ClientSessionState::Live => {
                // Remove from live packetizer - we'll feed from DVR instead
                self.live_packetizer
                    .remove_consumer(self.stitching_consumer.as_ref());
                // Create DVR player that feeds into the same stitching consumer
                let player = DvrPlayer::new(
                    self.source_id.clone(),
                    timestamp,
                    codec.clone(),
                    self.stitching_consumer.clone(),
                )?;
                let player = Arc::new(player);
                *state_guard = ClientSessionState::Dvr(player.clone(), None);
                let player_clone = player.clone();
                let join_handle = tokio::spawn(async move {
                    player_clone.play().await;
                });
                *state_guard = ClientSessionState::Dvr(player.clone(), Some(Arc::new(join_handle)));
            }
        };
        Ok(())
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
        if config.should_record {
            filesystem::ensure_stream_dvr_dir(&source_id)?;
            match DvrRecorder::new(&source_id) {
                Ok(res) => {
                    let recorder_arc = Arc::new(res);
                    rtsp_client.add_consumer(recorder_arc.clone()).await;
                    recorder.replace(recorder_arc);
                }
                Err(e) => {
                    log::error!("Failed to create recorder for stream {}: {}", source_id, e);
                }
            };
        }
        let rtsp_client_clone = rtsp_client.clone();
        let handle = tokio::spawn(async move {
            rtsp_client_clone.execute().await;
            ()
        });

        self.sources.insert(
            source_id.clone(),
            SourceWrapper {
                source: rtsp_client,
                task: handle,
                rtp_packetizer,
                recorder,
            },
        );

        log::info!("Stream {} created and started", source_id);
        Ok(source_id)
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

        let recording_start_time = match source.recorder.as_ref() {
            Some(recorder) => recorder.start_time(),
            None => None,
        };

        Ok(StreamInfo {
            source_id: source_id.clone(),
            rtsp_inputs: source.source.inputs().iter().cloned().collect(),
            state,
            should_record: source.source.config().should_record,
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
        sdp_request: CreateWebRtcSessionRequest,
        source_id: VideoSourceId,
    ) -> Result<(ClientSessionId, String)> {
        if let Some(source) = self.sources.get(&source_id).as_ref() {
            let packetizer = &source.rtp_packetizer;
            let codec_params = packetizer.get_codec_params();
            log::debug!(
                "create_websocket_session: source_id={}, codec_params={:?}",
                source_id,
                codec_params.is_some()
            );
            let codec_params = codec_params.ok_or_else(|| {
                anyhow::anyhow!("Codec parameters not found - stream may not be ready")
            })?;

            let (session, answer_sdp) = self
                .webrtc_manager
                .create_new_session(
                    source_id.clone(),
                    codec_params,
                    sdp_request,
                    self.downgrade(),
                )
                .await?;

            let session_id = session.id().clone();
            let client_session = ClientSession::new_live(
                session_id.clone(),
                source_id,
                session.clone(),
                packetizer.clone(),
            );
            // Add the stitching consumer to packetizer (not the raw session)
            packetizer.add_consumer(client_session.stitching_consumer.clone());
            self.client_sessions
                .insert(session_id.clone(), client_session);
            Ok((session_id, answer_sdp))
        } else {
            bail!("Stream not found")
        }
    }
    pub async fn delete_webrtc_session(&self, session_id: &ClientSessionId) -> anyhow::Result<()> {
        self.delete_client_session(session_id)?;
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
            let stitching_consumer = client_session.stitching_consumer.clone();
            self.client_sessions.insert(client_id, client_session);

            // Add the stitching consumer (which wraps the publisher) to the packetizer
            packetizer.add_consumer(stitching_consumer);
            Ok(publisher)
        } else {
            bail!("Stream not found")
        }
    }

    pub fn delete_client_session(&self, session_id: &ClientSessionId) -> anyhow::Result<()> {
        if let Some((_id, session)) = self.client_sessions.remove(session_id) {
            if let Some(ref source) = self.sources.get(session.source_id()) {
                // Remove the stitching consumer from the packetizer
                source
                    .rtp_packetizer
                    .remove_consumer(session.stitching_consumer.as_ref());
            } else {
                log::error!("Stream not found, shouldn't be possible");
            }
        } else {
            bail!("session not found");
        }
        Ok(())
    }

    pub async fn delete_wsc_rtp_session(&self, id: &ClientSessionId) -> Result<()> {
        self.delete_client_session(id)?;
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
            if let Some(source) = self.sources.get(session.source_id()).as_ref() {
                let codec = source.value().source.codec().await.clone().ok_or(anyhow!(
                    "source is not ready, no codec...\n try again later"
                ))?;
                return session.seek(timestamp, &codec).await;
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
            }),
            ClientSessionState::Dvr(player, _) => {
                let current_time = player.current_time_ms().await;
                Ok(media_server_api_models::SessionModeResponse {
                    is_live: false,
                    current_time_ms: current_time,
                    speed: player.speed(),
                })
            }
        }
    }

    pub async fn set_session_speed(&self, session_id: ClientSessionId, speed: f64) -> Result<()> {
        if let Some(ref session) = self.client_sessions.get(&session_id) {
            let state_guard = session.state.lock().await;
            if let ClientSessionState::Dvr(ref player, _) = *state_guard {
                player.set_speed(speed);
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
                    session_id: entry.key().clone(),
                    source_id: source_id.clone(),
                    is_live,
                });
            }
        }
        sessions
    }

    pub async fn cleanup(&self) -> Result<()> {
        log::info!("Starting cleanup of all streams...");

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
