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
    events: tokio::sync::watch::Sender<serde_json::Value>,
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
    ) -> Arc<Self> {
        let stitching_consumer = Arc::new(StitchingConsumer::new(publisher.clone()));
        let session = Arc::new(Self {
            events: tokio::sync::watch::channel(serde_json::json!({"is_live": true, "speed": 1.0, "current_time_ms": null, "reason": "initial"})).0,
            id,
            source_id,
            publisher,
            stitching_consumer,
            live_packetizer,
            state: tokio::sync::Mutex::new(ClientSessionState::Live),
        });
        let weak = Arc::downgrade(&session);
        let mut changes = session.events.subscribe();
        tokio::spawn(async move {
            loop {
                let Some(session) = weak.upgrade() else {
                    break;
                };
                let player = match &*session.state.lock().await {
                    ClientSessionState::Dvr(player) => Some(player.clone()),
                    ClientSessionState::Live => None,
                };
                drop(session);
                tokio::select! {
                    changed = changes.changed() => {
                        if changed.is_err() { break; }
                    }
                    _ = async {
                        if let Some(player) = player { player.catch_up_requested().await; }
                        else { std::future::pending::<()>().await; }
                    } => {}
                }
                let Some(session) = weak.upgrade() else {
                    break;
                };
                let mut state = session.state.lock().await;
                if let ClientSessionState::Dvr(player) = &*state
                    && player.take_catch_up()
                    && let Some(params) = session.live_packetizer.get_codec_params()
                {
                    if session.publisher.accepts_codec(params.codec()) {
                        session.go_live(&mut state, "caught_up").await;
                    } else {
                        player.require_live_codec(params.codec()).await;
                    }
                }
            }
        });
        session
    }

    pub async fn switch_to_live(&self) {
        let mut state = self.state.lock().await;
        self.go_live(&mut state, "manual").await;
    }

    async fn go_live(&self, state: &mut ClientSessionState, reason: &str) {
        if let ClientSessionState::Dvr(player) = state {
            if let Err(error) = player.terminate().await {
                log::error!("Failed to stop DVR: {error:#}");
            }
            let input = self.stitching_consumer.next_input().await;
            self.publisher
                .on_session_mode_change(SessionMode::Live)
                .await;
            *state = ClientSessionState::Live;
            self.events.send_replace(serde_json::json!({
                "is_live": true, "speed": 1.0, "current_time_ms": null, "reason": reason
            }));
            self.live_packetizer.add_consumer(input);
        }
    }

    pub async fn seek(&self, timestamp: UnixTimestamp) -> Result<()> {
        let mut state_guard = self.state.lock().await;
        match *state_guard {
            ClientSessionState::Dvr(ref player) => {
                player.seek_to_timestamp(timestamp).await?;
                self.publisher
                    .on_session_mode_change(SessionMode::Dvr {
                        timestamp: player.current_timestamp().await,
                    })
                    .await;
                self.events.send_replace(serde_json::json!({"is_live": false, "speed": player.speed(), "current_time_ms": player.current_timestamp().await, "reason": "seek"}));
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
                        self.events.send_replace(serde_json::json!({"is_live": false, "speed": player.speed(), "current_time_ms": player.current_timestamp().await, "reason": "seek"}));
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
    client_sessions: DashMap<ClientSessionId, Arc<ClientSession>>,
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
            rtsp_inputs: source.source.inputs().to_vec(),
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

    pub fn session_events(
        &self,
        id: ClientSessionId,
    ) -> Result<tokio::sync::watch::Receiver<serde_json::Value>> {
        self.client_sessions
            .get(&id)
            .map(|s| s.events.subscribe())
            .ok_or_else(|| anyhow!("Session not found"))
    }

    pub async fn set_session_speed(&self, session_id: ClientSessionId, speed: f64) -> Result<()> {
        if let Some(ref session) = self.client_sessions.get(&session_id) {
            let state_guard = session.state.lock().await;
            if let ClientSessionState::Dvr(ref player) = *state_guard {
                player.set_speed(speed)?;
                session.events.send_replace(serde_json::json!({"is_live": false, "speed": speed, "current_time_ms": player.current_timestamp().await, "reason": "speed_changed"}));
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
mod speed_transition_tests {
    use super::*;
    use crate::common::{VideoCodec, rtp::RtpPacket};
    use crate::domain::dvr::segments::{Frame, Session};
    use async_trait::async_trait;

    struct Publisher {
        id: ClientSessionId,
        source: String,
        modes: parking_lot::Mutex<Vec<SessionMode>>,
    }
    #[async_trait]
    impl RtpConsumer for Publisher {
        fn id(&self) -> &ClientSessionId {
            &self.id
        }
        async fn on_new_packet(&self, _: Arc<RtpPacket>) {}
    }
    #[async_trait]
    impl RtpVideoPublisher for Publisher {
        fn source_id(&self) -> &VideoSourceId {
            &self.source
        }
        async fn on_session_mode_change(&self, mode: SessionMode) {
            self.modes.lock().push(mode);
        }
    }

    #[tokio::test]
    async fn catch_up_handoff_notifies_live_one_x_and_reconnect_gets_snapshot() {
        let source = format!("catch-up-{}", uuid::Uuid::new_v4());
        let directory = std::env::temp_dir().join(&source);
        let recording =
            Session::in_directory(&source, 1000, VideoCodec::H264, directory.clone()).unwrap();
        recording.push(Frame {
            pts_ns: 0,
            dts_ns: 0,
            key: true,
            data: Vec::new().into(),
        });
        let publisher = Arc::new(Publisher {
            id: uuid::Uuid::new_v4(),
            source: source.clone(),
            modes: Default::default(),
        });
        let session = ClientSession::new_live(
            publisher.id,
            source.clone(),
            publisher.clone(),
            Arc::new(RtpPacketizer::new(1, 96)),
        );
        let player = Arc::new(
            DvrPlayer::new(source, 1000, session.stitching_consumer.input())
                .await
                .unwrap(),
        );
        player.set_speed(4.0).unwrap();
        let mut events = session.events.subscribe();
        let mut state = session.state.lock().await;
        *state = ClientSessionState::Dvr(player);
        session.go_live(&mut state, "caught_up").await;
        assert!(matches!(*state, ClientSessionState::Live));
        assert!(events.has_changed().unwrap());
        let value = events.borrow_and_update().clone();
        assert_eq!(value["is_live"], true);
        assert_eq!(value["speed"], 1.0);
        assert_eq!(value["reason"], "caught_up");
        assert!(value["current_time_ms"].is_null());
        assert_eq!(*session.events.subscribe().borrow(), value);
        session.go_live(&mut state, "caught_up").await;
        assert_eq!(publisher.modes.lock().len(), 1);
        drop(state);
        drop(session);
        drop(recording);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
