use crate::common::rtp::{CodecParameters, RtpPacket, RtpPacketizer, StitchingConsumer};
use crate::common::traits::{RtpConsumer, RtpVideoPublisher, VideoSource};
use crate::common::VideoCodec;
use crate::domain::dvr::{filesystem, recorder::DvrRecorder};
use crate::domain::{StreamConfig, StreamInfo, StreamState};
use crate::publishers::webrtc::{WebRtcSession, WebrtcManager};
use crate::publishers::wsc_rtp::{WscRtpPublisher, WscRtpUdpManager};
use crate::sources::dvr::DvrPlayer;
use crate::sources::rtsp::RtspClient;
use crate::utils::UnixTimestamp;
use anyhow::{anyhow, bail, Result};
use dashmap::DashMap;
use gstreamer::glib::clone::Downgrade;
use media_server_api_models::CreateWebRtcSessionRequest;
use parking_lot::Mutex;
use serde::Deserialize;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Weak};
use tokio::net::UdpSocket;
use tokio::sync::broadcast;
use uuid::Uuid;

struct SourceWrapper {
    source: Arc<dyn VideoSource>,
    task: tokio::task::JoinHandle<()>,
    rtp_packetizer: Arc<RtpPacketizer>,
}

pub type ClientSessionId = Uuid;
pub type VideoSourceId = u64;
pub type SharedGlobalState = Arc<GlobalState>;
pub type WeakGlobalState = Weak<GlobalState>;

#[derive(Deserialize, Debug)]
pub struct AppSettings {
    #[serde(default = "default_wsc_holepunch_port")]
    pub wsc_holepunch_port: u16,
}

fn default_wsc_holepunch_port() -> u16 {
    5000
}

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
            ClientSessionState::Dvr(ref player, ref handle_opt) => {
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
        let player = match *state_guard {
            ClientSessionState::Dvr(ref player, _) => player.clone(),
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
                player
            }
        };
        drop(state_guard);

        let player_clone = player.clone();
        let join_handle = tokio::spawn(async move {
            player_clone.play().await;
        });

        // Update the state with the new player and handle
        let mut state_guard = self.state.lock().await;
        *state_guard = ClientSessionState::Dvr(player, Some(Arc::new(join_handle)));
        Ok(())
    }
}
pub struct GlobalState {
    sources: DashMap<VideoSourceId, SourceWrapper>,
    client_sessions: DashMap<ClientSessionId, ClientSession>,
    wsc_manager: Arc<WscRtpUdpManager>,
    extra_shutdown_tasks: Vec<tokio::task::JoinHandle<()>>,
    webrtc_manager: WebrtcManager,
    settings: AppSettings,
}

impl GlobalState {
    pub async fn new() -> anyhow::Result<Self> {
        let settings = AppSettings::new()?;
        let wsc_rtp_listener = Arc::new(WscRtpUdpManager::new(&settings).await?);
        let wsc_rtp_listener_clone = wsc_rtp_listener.clone();
        tokio::spawn(async move { wsc_rtp_listener_clone.run().await });
        let webrtc_manager = WebrtcManager::new()?;

        let res = Self {
            sources: DashMap::new(),
            client_sessions: DashMap::new(),
            wsc_manager: wsc_rtp_listener,
            extra_shutdown_tasks: Vec::new(),
            webrtc_manager,
            settings: settings,
        };
        Ok(res)
    }

    pub fn settings(&self) -> &AppSettings {
        &self.settings
    }

    pub async fn create_rtsp_stream(&self, config: StreamConfig) -> Result<u64> {
        let source_id = config.source_id;

        if self.sources.contains_key(&source_id) {
            bail!("Stream {} already exists", source_id);
        }

        for entry in self.sources.iter() {
            if entry.value().source.url() == &config.rtsp_url {
                bail!(
                    "RTSP URL {} already exists with source id {}",
                    config.rtsp_url,
                    entry.value().source.source_id()
                );
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

        if config.should_record {
            filesystem::ensure_stream_dvr_dir(source_id)?;
            match DvrRecorder::new(source_id) {
                Ok(recorder) => {
                    rtsp_client.add_consumer(Arc::new(recorder)).await;
                }
                Err(e) => {
                    log::error!("Failed to create recorder for stream {}: {}", source_id, e);
                }
            }
        }
        let rtsp_client_clone = rtsp_client.clone();
        let handle = tokio::spawn(async move {
            rtsp_client_clone.execute().await;
            ()
        });

        self.sources.insert(
            source_id,
            SourceWrapper {
                source: rtsp_client,
                task: handle,
                rtp_packetizer,
            },
        );

        log::info!("Stream {} created and started", source_id);
        Ok(source_id)
    }

    pub async fn delete_stream(&self, source_id: u64) -> Result<()> {
        if let Some((_, src)) = self.sources.remove(&source_id) {
            let _ = src.source.stop().await;
            src.task.abort();

            log::info!("Stream {} and all its sessions ", source_id);
        }
        Ok(())
    }

    pub async fn get_stream(&self, source_id: u64) -> Result<StreamInfo> {
        let source = self
            .sources
            .get(&source_id)
            .ok_or_else(|| anyhow!("Stream {} not found", source_id))?;

        let state = source.source.state().await;
        let webrtc_sessions: Vec<Uuid> = self
            .client_sessions
            .iter()
            .filter(|entry| entry.value().source_id() == &source_id)
            .map(|entry| entry.key().clone())
            .collect();

        Ok(StreamInfo {
            session_id: source_id,
            rtsp_url: source.source.url().to_string(),
            state,
            should_record: source.source.config().should_record,
            webrtc_sessions,
            restart_interval_secs: source.source.config().restart_interval_secs,
        })
    }

    pub async fn list_streams(&self) -> Vec<StreamInfo> {
        let mut streams = Vec::new();
        for entry in self.sources.iter() {
            if let Ok(info) = self.get_stream(*entry.key()).await {
                streams.push(info);
            }
        }
        streams
    }

    pub async fn subscribe_stream_state(
        &self,
        source_id: u64,
    ) -> Result<broadcast::Receiver<StreamState>> {
        let source = self
            .sources
            .get(&source_id)
            .ok_or_else(|| anyhow!("Stream {} not found", source_id))?;

        Ok(source.source.subscribe_state())
    }

    pub async fn create_websocket_session(
        self: Arc<Self>,
        sdp_request: CreateWebRtcSessionRequest,
        source_id: VideoSourceId,
    ) -> Result<(ClientSessionId, String)> {
        if let Some(source) = self.sources.get(&source_id).as_ref() {
            let codec = source.source.codec().await;
            log::debug!(
                "create_websocket_session: source_id={}, codec={:?}",
                source_id,
                codec
            );
            let codec = codec
                .ok_or_else(|| anyhow::anyhow!("Codec not found - stream may not be ready"))?;
            let packetizer = &source.rtp_packetizer;
            let codec_params = packetizer.get_codec_params();
            log::debug!(
                "create_websocket_session: codec_params={:?}",
                codec_params.is_some()
            );
            let codec_params = codec_params.ok_or_else(|| {
                anyhow::anyhow!("Codec parameters not found - stream may not be ready")
            })?;

            let (session, answer_sdp) = self
                .webrtc_manager
                .create_new_session(
                    source_id,
                    codec,
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
        client_port: Option<u16>,
    ) -> Result<ClientSessionId> {
        if let Some(source) = self.sources.get(&source_id).as_ref() {
            let packetizer = &source.rtp_packetizer;
            let publisher = Arc::new(WscRtpPublisher::new(source_id));
            let client_id = publisher.id().clone();

            // Register the publisher with the UDP manager for holepunching
            self.wsc_manager
                .register_publisher(client_id.clone(), publisher.clone());

            // Create and store the client session
            let client_session = ClientSession::new_live(
                client_id.clone(),
                source_id,
                publisher.clone(),
                packetizer.clone(),
            );
            let stitching_consumer = client_session.stitching_consumer.clone();
            self.client_sessions
                .insert(client_id.clone(), client_session);

            // Add the stitching consumer (which wraps the publisher) to the packetizer
            packetizer.add_consumer(stitching_consumer);
            Ok(client_id)
        } else {
            bail!("Stream not found")
        }
    }

    pub async fn try_get_wsc_rtp_sdp_info(&self, client_session_id: &Uuid) -> Result<String> {
        let source_id = self
            .client_sessions
            .get(client_session_id)
            .map(|session| *session.value().source_id())
            .ok_or_else(|| anyhow::anyhow!("Stream not found"))?;
        let source = self
            .sources
            .get(&source_id)
            .ok_or_else(|| anyhow::anyhow!("Stream not found"))?;
        let codec = source
            .source
            .codec()
            .await
            .ok_or(anyhow!("source has no codec set"))?;
        let codec_params = source
            .rtp_packetizer
            .get_codec_params()
            .ok_or(anyhow!("source has no RTP packetizer"))?;
        self.wsc_manager
            .get_sdp_for_session(client_session_id, &codec, &codec_params)
            .ok_or(anyhow!("failed to get "))
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
        self.wsc_manager.remove_publisher(id);
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

    pub async fn seek_webrtc_session(
        &self,
        _source_id: VideoSourceId,
        session_id: ClientSessionId,
        timestamp: UnixTimestamp,
    ) -> Result<()> {
        self.seek_session(&session_id, timestamp).await
    }

    pub async fn get_session_mode(
        &self,
        _source_id: VideoSourceId,
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

    pub async fn set_session_speed(
        &self,
        _source_id: VideoSourceId,
        session_id: ClientSessionId,
        speed: f64,
    ) -> Result<()> {
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
        source_id: VideoSourceId,
    ) -> Vec<media_server_api_models::WebRtcSessionResponse> {
        let mut sessions = Vec::new();
        for entry in self.client_sessions.iter() {
            if entry.value().source_id() == &source_id {
                let state = entry.value().state.lock().await;
                let is_live = matches!(*state, ClientSessionState::Live);
                sessions.push(media_server_api_models::WebRtcSessionResponse {
                    session_id: entry.key().clone(),
                    source_id,
                    is_live,
                });
            }
        }
        sessions
    }

    pub fn wsc_manager(&self) -> &WscRtpUdpManager {
        &self.wsc_manager
    }

    pub async fn cleanup(&self) -> Result<()> {
        log::info!("Starting cleanup of all streams...");

        let source_ids: Vec<u64> = self.sources.iter().map(|entry| *entry.key()).collect();

        for source_id in source_ids {
            log::info!("Stopping stream {} during cleanup", source_id);
            if let Err(e) = self.delete_stream(source_id).await {
                log::error!("Error stopping stream {} during cleanup: {}", source_id, e);
            }
        }

        log::info!("Cleanup complete");
        Ok(())
    }
}

fn should_override_wsc_rtp_port(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(addr) => {
            addr.is_loopback() || addr.is_private() || addr.is_link_local()
        }
        std::net::IpAddr::V6(addr) => {
            addr.is_loopback() || addr.is_unique_local() || addr.is_unicast_link_local()
        }
    }
}
