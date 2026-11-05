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
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};
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
    live_packetizer: Option<Arc<RtpPacketizer>>,
    clip: Option<crate::domain::dvr::import_normalize::ImportResult>,
    ready: AtomicBool,
    terminated: AtomicBool,
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
            live_packetizer: Some(live_packetizer),
            clip: None,
            ready: AtomicBool::new(true),
            terminated: AtomicBool::new(false),
            state: tokio::sync::Mutex::new(ClientSessionState::Live),
        }
    }

    async fn new_clip(
        id: ClientSessionId,
        source_id: VideoSourceId,
        publisher: Arc<dyn RtpVideoPublisher>,
        clip: crate::domain::dvr::import_normalize::ImportResult,
        timestamp: Option<u64>,
    ) -> Result<Self> {
        let timestamp = timestamp
            .unwrap_or(clip.actual_start_ms)
            .clamp(clip.actual_start_ms, clip.actual_end_ms);
        publisher.update_params(&DvrPlayer::codec_parameters(&source_id, timestamp).await?);
        let stitching_consumer = Arc::new(StitchingConsumer::new(publisher.clone()));
        let player = Arc::new(
            DvrPlayer::new(source_id.clone(), timestamp, stitching_consumer.input()).await?,
        );
        Ok(Self {
            id,
            source_id,
            publisher,
            live_packetizer: None,
            clip: Some(clip),
            ready: AtomicBool::new(false),
            terminated: AtomicBool::new(false),
            stitching_consumer,
            state: tokio::sync::Mutex::new(ClientSessionState::Dvr(player)),
        })
    }

    async fn transport_ready(&self) -> Result<()> {
        let state = self.state.lock().await;
        if self.terminated.load(Ordering::Acquire) || self.ready.load(Ordering::Acquire) {
            return Ok(());
        }
        if let ClientSessionState::Dvr(player) = &*state {
            player.play().await?;
            self.publisher
                .on_session_mode_change(SessionMode::Dvr {
                    timestamp: player.current_timestamp().await,
                })
                .await;
        }
        self.ready.store(true, Ordering::Release);
        Ok(())
    }

    pub async fn switch_to_live(&self) -> Result<()> {
        let live_packetizer = self
            .live_packetizer
            .as_ref()
            .ok_or_else(|| anyhow!("Finite clips have no live input"))?;
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
            live_packetizer.add_consumer(input);
            *state = ClientSessionState::Live;
        }
        Ok(())
    }

    pub async fn seek(&self, timestamp: UnixTimestamp) -> Result<()> {
        let mut state_guard = self.state.lock().await;
        anyhow::ensure!(!self.terminated.load(Ordering::Acquire), "Session closed");
        let timestamp = self.clip.as_ref().map_or(timestamp, |clip| {
            timestamp.clamp(clip.actual_start_ms, clip.actual_end_ms)
        });
        match *state_guard {
            ClientSessionState::Dvr(ref player) => {
                if self.ready.load(Ordering::Acquire) {
                    player.seek_to_timestamp(timestamp, 1.0).await?;
                } else {
                    let prepared = Arc::new(
                        DvrPlayer::new(
                            self.source_id.clone(),
                            timestamp,
                            self.stitching_consumer.input(),
                        )
                        .await?,
                    );
                    *state_guard = ClientSessionState::Dvr(prepared);
                    return Ok(());
                }
                self.publisher
                    .on_session_mode_change(SessionMode::Dvr {
                        timestamp: player.current_timestamp().await,
                    })
                    .await;
                Ok(())
            }
            ClientSessionState::Live => {
                let live_packetizer = self
                    .live_packetizer
                    .as_ref()
                    .ok_or_else(|| anyhow!("No live input"))?;
                // Validate timestamp before switching mode — don't enter DVR if no recording exists
                if !DvrPlayer::available(&self.source_id, timestamp) {
                    bail!("No recording found for timestamp {}", timestamp);
                }
                // Create DVR player that feeds into the same stitching consumer
                live_packetizer.remove_consumer(self.stitching_consumer.as_ref());
                let input = self.stitching_consumer.next_input().await;
                let player = match DvrPlayer::new(self.source_id.clone(), timestamp, input).await {
                    Ok(player) => Arc::new(player),
                    Err(error) => {
                        live_packetizer.add_consumer(self.stitching_consumer.next_input().await);
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
                        live_packetizer.add_consumer(self.stitching_consumer.next_input().await);
                        Err(e)
                    }
                }
            }
        }
    }

    async fn terminate(&self) {
        let state = self.state.lock().await;
        self.terminated.store(true, Ordering::Release);
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
/// The oldest retained DVR media can predate the current recorder after restart.
async fn earliest_recording_time(
    source_id: VideoSourceId,
    active_start: Option<u64>,
) -> Result<Option<u64>> {
    let disk_start = tokio::task::spawn_blocking(move || {
        crate::domain::dvr::segments::list(&source_id)
            .map(|segments| segments.iter().map(|segment| segment.start_ms()).min())
    })
    .await??;
    Ok(disk_start.into_iter().chain(active_start).min())
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
        let recording_start_time = earliest_recording_time(
            source_id.clone(),
            recorder.as_ref().and_then(|r| r.start_time()),
        )
        .await?;

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

    async fn clip_info(
        source_id: &str,
    ) -> Result<Option<crate::domain::dvr::import_normalize::ImportResult>> {
        if !source_id.starts_with("prealarm:") {
            return Ok(None);
        }
        let directory = filesystem::get_stream_dvr_dir(&source_id.to_owned());
        let info = tokio::task::spawn_blocking(move || {
            crate::domain::dvr::import_jobs::completed_in(&directory)
        })
        .await??;
        Ok(Some(
            info.ok_or_else(|| anyhow!("Clip not found or expired"))?,
        ))
    }

    pub async fn session_transport_ready(&self, id: &ClientSessionId) -> Result<()> {
        let session = self
            .client_sessions
            .get(id)
            .map(|s| s.clone())
            .ok_or_else(|| anyhow!("Session not found"))?;
        session.transport_ready().await
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
        let clip = Self::clip_info(&source_id).await?;
        let packetizer = self
            .sources
            .get(&source_id)
            .map(|source| source.rtp_packetizer.clone());
        let timestamp = clip
            .as_ref()
            .map(|clip| {
                timestamp
                    .unwrap_or(clip.actual_start_ms)
                    .clamp(clip.actual_start_ms, clip.actual_end_ms)
            })
            .or(timestamp);
        let params = if let Some(timestamp) = timestamp {
            DvrPlayer::codec_parameters(&source_id, timestamp).await?
        } else {
            packetizer
                .as_ref()
                .ok_or_else(|| anyhow!("Stream not found"))?
                .get_codec_params()
                .ok_or_else(|| anyhow!("Codec parameters not found - stream may not be ready"))?
        };
        let (publisher, answer) = self
            .webrtc_manager
            .create_new_session(source_id.clone(), params, request, self.downgrade())
            .await?;
        let id = *publisher.id();
        if let Some(clip) = clip {
            let session =
                match ClientSession::new_clip(id, source_id, publisher.clone(), clip, timestamp)
                    .await
                {
                    Ok(session) => session,
                    Err(error) => {
                        let _ = self.webrtc_manager.delete_session(&id).await;
                        return Err(error);
                    }
                };
            self.client_sessions.insert(id, Arc::new(session));
            if publisher.connected() {
                self.session_transport_ready(&id).await?;
            }
            return Ok((id, answer));
        }
        let packetizer = packetizer.ok_or_else(|| anyhow!("Stream not found"))?;
        let session = ClientSession::new_live(id, source_id, publisher, packetizer.clone());
        if let Some(timestamp) = timestamp {
            if let Err(error) = session.seek(timestamp).await {
                let _ = self.webrtc_manager.delete_session(&id).await;
                return Err(error);
            }
        } else {
            packetizer.add_consumer(session.stitching_consumer.input());
        }
        self.client_sessions.insert(id, Arc::new(session));
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
        if let Some(clip) = Self::clip_info(&source_id).await? {
            let publisher = Arc::new(WscRtpPublisher::new(source_id.clone()));
            let id = *publisher.id();
            let session =
                ClientSession::new_clip(id, source_id, publisher.clone(), clip, None).await?;
            self.client_sessions.insert(id, Arc::new(session));
            self.wsc_publishers.insert(id, publisher.clone());
            return Ok(publisher);
        }
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
            self.client_sessions
                .insert(client_id, Arc::new(client_session));

            // Add the stitching consumer (which wraps the publisher) to the packetizer
            packetizer.add_consumer(stitching_consumer);
            Ok(publisher)
        } else {
            bail!("Stream not found")
        }
    }

    pub async fn delete_client_session(&self, session_id: &ClientSessionId) -> anyhow::Result<()> {
        if let Some((_id, session)) = self.client_sessions.remove(session_id) {
            if let Some(packetizer) = &session.live_packetizer {
                packetizer.remove_consumer(session.stitching_consumer.as_ref());
            }
            session.terminate().await;
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
        if let Some(session) = self.client_sessions.get(id).map(|s| Arc::clone(s.value())) {
            return Ok(session.state.lock().await.clone());
        }
        bail!("Session not found");
    }

    pub async fn switch_session_to_live(&self, session_id: &ClientSessionId) -> Result<()> {
        if let Some(session) = self
            .client_sessions
            .get(session_id)
            .map(|s| Arc::clone(s.value()))
        {
            return session.switch_to_live().await;
        }
        bail!("Session not found");
    }

    pub async fn seek_session(
        &self,
        session_id: &ClientSessionId,
        timestamp: UnixTimestamp,
    ) -> Result<()> {
        if let Some(session) = self
            .client_sessions
            .get(session_id)
            .map(|s| Arc::clone(s.value()))
        {
            return session.seek(timestamp).await;
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
        if let Some(session) = self
            .client_sessions
            .get(&session_id)
            .map(|s| Arc::clone(s.value()))
        {
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

#[cfg(test)]
mod clip_session_tests {
    use super::*;
    use crate::domain::dvr::{import_normalize, segments};
    use media_server_api_models::PlaybackEvent;
    use tokio::time::{Duration, timeout};

    struct Clip(String);
    impl Clip {
        fn new(hevc: bool) -> Self {
            let id = format!("prealarm:session-test:{}", uuid::Uuid::new_v4());
            let directory = filesystem::get_stream_dvr_dir(&id);
            let input = std::env::temp_dir().join(format!("{}.mp4", uuid::Uuid::new_v4()));
            import_normalize::tests::fixture(&input, hevc, false);
            import_normalize::normalize(
                &input,
                &directory.join("segments-test"),
                1_700_000_000_000,
                &tokio_util::sync::CancellationToken::new(),
            )
            .unwrap();
            std::fs::remove_file(input).unwrap();
            Self(id)
        }
    }
    impl Drop for Clip {
        fn drop(&mut self) {
            let directory = filesystem::get_stream_dvr_dir(&self.0);
            segments::forget_stream(&directory);
            let _ = std::fs::remove_dir_all(directory);
        }
    }

    async fn ended(state: &GlobalState, id: ClientSessionId) {
        timeout(Duration::from_secs(5), async {
            loop {
                let mode = state.get_session_mode(id).await.unwrap();
                assert!(!mode.is_live);
                match mode.playback.unwrap() {
                    PlaybackEvent::Ended { .. } => break,
                    PlaybackEvent::Error { message, .. } => panic!("{message}"),
                    _ => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn clips_wait_for_transport_seek_replay_and_cleanup_without_live_sources() {
        for hevc in [false, true] {
            let clip = Clip::new(hevc);
            segments::forget_stream(&filesystem::get_stream_dvr_dir(&clip.0));
            let state = GlobalState::new().await.unwrap();
            let publisher = state
                .create_wsc_rtp_registration(clip.0.clone())
                .await
                .unwrap();
            let id = *publisher.id();
            assert!(
                publisher
                    .get_sdp()
                    .unwrap()
                    .contains(if hevc { "H265" } else { "H264" })
            );
            assert!(state.sources.is_empty());
            assert!(state.switch_session_to_live(&id).await.is_err());
            state.seek_session(&id, 0).await.unwrap();
            assert_eq!(
                state.get_session_mode(id).await.unwrap().current_time_ms,
                Some(1_700_000_000_000)
            );
            state.seek_session(&id, 1_700_000_006_500).await.unwrap();
            let middle = state
                .get_session_mode(id)
                .await
                .unwrap()
                .current_time_ms
                .unwrap();
            assert!((1_700_000_005_500..=1_700_000_006_500).contains(&middle));
            state.seek_session(&id, u64::MAX).await.unwrap();
            let position = state.get_session_mode(id).await.unwrap().current_time_ms;
            tokio::time::sleep(Duration::from_millis(150)).await;
            assert_eq!(
                state.get_session_mode(id).await.unwrap().current_time_ms,
                position
            );
            state.session_transport_ready(&id).await.unwrap();
            state.session_transport_ready(&id).await.unwrap();
            ended(&state, id).await;
            state.seek_session(&id, u64::MAX).await.unwrap();
            ended(&state, id).await;
            let session = state.client_sessions.get(&id).unwrap().clone();
            state.delete_wsc_rtp_session(&id).await.unwrap();
            state.delete_wsc_rtp_session(&id).await.unwrap();
            session.transport_ready().await.unwrap();
            assert!(session.seek(1_700_000_000_000).await.is_err());
            assert!(state.client_sessions.is_empty());
            assert!(state.wsc_publishers.is_empty());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn actual_websocket_route_streams_a_clip_with_legacy_and_opt_in_protocol() {
        let clip = Clip::new(false);
        let state = Arc::new(GlobalState::new().await.unwrap());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = crate::api::create_router(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });
        for (events, force_ws, udp) in [
            (false, true, false),
            (true, true, false),
            (true, false, false),
            (true, false, true),
        ] {
            let source = clip.0.clone();
            let socket_state = state.clone();
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                let (mut socket, _) = tungstenite::connect(format!("ws://{addr}/streams/{source}/wsc-rtp?force_websocket_transport={force_ws}&playback_events={events}")).unwrap();
                if let tungstenite::stream::MaybeTlsStream::Plain(stream) = socket.get_mut() {
                    stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                }
                let mut id = None;
                let mut udp_socket = None;
                let mut sdp = false;
                let mut mode = false;
                let mut rtp = 0;
                loop {
                    let message = match socket.read() {
                        Ok(message) => message,
                        Err(tungstenite::Error::Io(error)) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                            socket.send(tungstenite::Message::Text(r#"{"type":"ping"}"#.into())).unwrap();
                            continue;
                        }
                        Err(error) => panic!("{error}"),
                    };
                    match message {
                        tungstenite::Message::Text(text) => {
                            let message: serde_json::Value = serde_json::from_str(&text).unwrap();
                            match message["type"].as_str().unwrap() {
                                "init" => { assert!(message["active_source"].is_null()); id = Some(uuid::Uuid::parse_str(message["session_id"].as_str().unwrap()).unwrap());
                                    if udp {
                                        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                                        socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                                        let addr = format!("127.0.0.1:{}", message["holepunch_port"].as_u64().unwrap());
                                        socket.send_to(format!("ws-rtp {}", id.unwrap()).as_bytes(), &addr).unwrap();
                                        let mut packet = [0u8; 2048];
                                        let (size, _) = socket.recv_from(&mut packet).unwrap();
                                        assert!(std::str::from_utf8(&packet[..size]).unwrap().starts_with("ws-rtp-dummy"));
                                        socket.send_to(format!("ws-rtp-ack {}", id.unwrap()).as_bytes(), &addr).unwrap();
                                        udp_socket = Some(socket);
                                    } }
                                "sdp" => { assert!(message["sdp"].as_str().unwrap().contains("H264")); sdp = true; }
                                "session_mode" => {
                                    mode = true;
                                    if let Some(socket) = &udp_socket {
                                        assert!(sdp);
                                        for _ in 0..3 {
                                            let mut bytes = [0u8; 2048];
                                            assert!(socket.recv_from(&mut bytes).unwrap().0 >= 12);
                                        }
                                        break;
                                    }
                                }
                                "falling_back_rtp_to_ws" => {}
                                _ => {}
                            }
                        }
                        tungstenite::Message::Binary(bytes) => { assert!(sdp && mode); assert!(bytes.len() >= 12); rtp += 1; if rtp == 3 { break; } }
                        _ => {}
                    }
                }
                handle.block_on(socket_state.seek_session(&id.unwrap(), u64::MAX)).unwrap();
                handle.block_on(ended(&socket_state, id.unwrap()));
                socket.close(None).unwrap();
            }).await.unwrap();
            timeout(Duration::from_secs(5), async {
                while !state.client_sessions.is_empty() || !state.wsc_publishers.is_empty() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
        assert!(state.wsc_publishers.is_empty());
        assert!(state.sources.is_empty());
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn webrtc_clip_negotiates_without_a_live_source_and_starts_when_connected() {
        use webrtc::{
            api::{APIBuilder, media_engine::MediaEngine},
            peer_connection::{
                configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription,
            },
            rtp_transceiver::{
                RTCRtpTransceiverInit, rtp_codec::RTPCodecType,
                rtp_transceiver_direction::RTCRtpTransceiverDirection,
            },
        };
        let clip = Clip::new(false);
        let state = Arc::new(GlobalState::new().await.unwrap());
        assert!(
            state
                .clone()
                .create_websocket_session(
                    CreateWebRtcSessionRequest {
                        offer: "invalid".into()
                    },
                    clip.0.clone()
                )
                .await
                .is_err()
        );
        assert!(state.client_sessions.is_empty());
        let mut engine = MediaEngine::default();
        engine.register_default_codecs().unwrap();
        let peer = APIBuilder::new()
            .with_media_engine(engine)
            .build()
            .new_peer_connection(RTCConfiguration::default())
            .await
            .unwrap();
        peer.add_transceiver_from_kind(
            RTPCodecType::Video,
            Some(RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        )
        .await
        .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        peer.on_track(Box::new(move |track, _, _| {
            let tx = tx.clone();
            Box::pin(async move {
                if let Ok((packet, _)) = track.read_rtp().await {
                    let _ = tx.send(packet).await;
                }
            })
        }));
        let offer = peer.create_offer(None).await.unwrap();
        let mut gathering = peer.gathering_complete_promise().await;
        peer.set_local_description(offer).await.unwrap();
        gathering.recv().await;
        let offer = peer.local_description().await.unwrap().sdp;
        let (id, answer) = timeout(
            Duration::from_secs(20),
            state.clone().create_dvr_webrtc_session(
                CreateWebRtcSessionRequest { offer },
                clip.0.clone(),
                1_700_000_012_300,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let initial = state.get_session_mode(id).await.unwrap().current_time_ms;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            state.get_session_mode(id).await.unwrap().current_time_ms,
            initial
        );
        peer.set_remote_description(RTCSessionDescription::answer(answer).unwrap())
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_secs(10), rx.recv())
                .await
                .unwrap()
                .is_some()
        );
        ended(&state, id).await;
        state.delete_webrtc_session(&id).await.unwrap();
        peer.close().await.unwrap();
        assert!(state.client_sessions.is_empty());
        assert!(state.sources.is_empty());
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn incomplete_corrupt_and_expired_clips_do_not_leak_sessions() {
        let state = GlobalState::new().await.unwrap();
        let clip = Clip::new(false);
        let directory = filesystem::get_stream_dvr_dir(&clip.0);
        let marker = directory.join(".import-staging");
        std::fs::write(&marker, b"").unwrap();
        assert!(
            state
                .create_wsc_rtp_registration(clip.0.clone())
                .await
                .is_err()
        );
        std::fs::remove_file(marker).unwrap();
        let files = segments::list(&clip.0).unwrap();
        std::fs::write(&files[0].path, b"invalid mp4").unwrap();
        assert!(
            state
                .create_wsc_rtp_registration(clip.0.clone())
                .await
                .is_err()
        );
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(
            state
                .create_wsc_rtp_registration(clip.0.clone())
                .await
                .is_err()
        );
        std::fs::create_dir_all(&directory).unwrap();
        segments::forget_stream(&directory);
        assert!(
            state
                .create_wsc_rtp_registration(clip.0.clone())
                .await
                .is_err()
        );
        assert!(state.client_sessions.is_empty());
        assert!(state.wsc_publishers.is_empty());
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn availability_uses_retained_recordings_before_a_restarted_recorder() {
        let clip = Clip::new(false);
        assert_eq!(
            earliest_recording_time(clip.0.clone(), Some(1_700_000_100_000))
                .await
                .unwrap(),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            earliest_recording_time(clip.0.clone(), None).await.unwrap(),
            Some(1_700_000_000_000)
        );
        let missing = format!("availability-test:{}", uuid::Uuid::new_v4());
        assert_eq!(
            earliest_recording_time(missing.clone(), None)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            earliest_recording_time(missing, Some(500)).await.unwrap(),
            Some(500)
        );
    }

    #[tokio::test]
    async fn missing_clips_do_not_leave_registrations() {
        let state = GlobalState::new().await.unwrap();
        for id in ["prealarm:missing:clip", "unknown-live"] {
            assert!(state.create_wsc_rtp_registration(id.into()).await.is_err());
        }
        assert!(state.client_sessions.is_empty());
        assert!(state.wsc_publishers.is_empty());
    }
}
