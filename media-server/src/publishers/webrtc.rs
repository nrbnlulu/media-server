use anyhow::{bail, Result};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use dashmap::DashMap;
use media_server_api_models::CreateWebRtcSessionRequest;
use std::sync::Arc;
use uuid::Uuid;
use webrtc::{
    peer_connection::{
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription, RTCPeerConnection,
    },
    track::track_local::{track_local_static_rtp::TrackLocalStaticRTP, TrackLocalWriter},
};

use crate::{
    app::{ClientSessionId, VideoSourceId, WeakGlobalState},
    common::{
        rtp::{CodecParameters, RtpPacket},
        traits::{RtpConsumer, RtpVideoPublisher},
        VideoCodec,
    },
};

pub struct WebrtcManager {
    api: webrtc::api::API,
    sessions: DashMap<ClientSessionId, Arc<WebRtcSession>>,
}

impl WebrtcManager {
    pub fn new() -> anyhow::Result<Self> {
        let mut m = webrtc::api::media_engine::MediaEngine::default();
        m.register_default_codecs()?;

        let mut registry = webrtc::interceptor::registry::Registry::new();
        registry =
            webrtc::api::interceptor_registry::register_default_interceptors(registry, &mut m)?;

        let api = webrtc::api::APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .build();

        Ok(Self {
            api,
            sessions: DashMap::new(),
        })
    }

    pub async fn create_new_session(
        &self,
        source_id: VideoSourceId,
        codec: VideoCodec,
        params: CodecParameters,
        request: CreateWebRtcSessionRequest,
        app_state: WeakGlobalState,
    ) -> anyhow::Result<(Arc<WebRtcSession>, String)> {
        let config = webrtc::peer_connection::configuration::RTCConfiguration {
            ice_servers: vec![webrtc::ice_transport::ice_server::RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let session_id = uuid::Uuid::new_v4();
        let session_id_clone = session_id.clone();
        let mut conn = self.api.new_peer_connection(config).await?;
        conn.on_peer_connection_state_change(Box::new(move |state| {
            let app_state_clone = app_state.clone();
            Box::pin(async move {
                match state {
                    RTCPeerConnectionState::Closed
                    | RTCPeerConnectionState::Failed
                    | RTCPeerConnectionState::Disconnected => {
                        log::info!("WebRTC session closed");
                        if let Some(app_state) = app_state_clone.upgrade() {
                            let _ = app_state.delete_webrtc_session(&session_id_clone).await;
                        }
                    }
                    _ => {}
                }
            })
        }));
        let track = Arc::new(
            webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP::new(
                webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability {
                    mime_type: codec.mime_type().to_owned(),
                    clock_rate: 90000,
                    ..Default::default()
                },
                "live_stream".to_string(),
                session_id.to_string(),
            ),
        );

        conn.add_track(track.clone()).await?;

        let offer = RTCSessionDescription::offer(request.offer)?;
        log::debug!(
            "Offer SDP type: {:?}, sdp length: {}",
            offer.sdp_type,
            offer.sdp.len()
        );

        log::debug!("Setting remote description (offer)");
        conn.set_remote_description(offer).await?;

        log::debug!("Creating answer");
        let answer = conn.create_answer(None).await?;
        log::debug!(
            "Answer created, type: {:?}, sdp length: {}",
            answer.sdp_type,
            answer.sdp.len()
        );

        log::debug!("Setting local description (answer)");
        conn.set_local_description(answer.clone()).await?;

        log::debug!("Waiting for ICE gathering to complete");
        conn.gathering_complete_promise().await.recv().await;
        log::info!("ICE gathering complete for session {}", session_id);

        let answer_sdp = if let Some(local_desc) = conn.local_description().await {
            log::debug!(
                "Final answer SDP with ICE candidates, length: {}",
                local_desc.sdp.len()
            );
            local_desc.sdp
        } else {
            bail!("No local description available");
        };
        let session = Arc::new(WebRtcSession::new(source_id, track, session_id, conn));
        self.sessions.insert(session_id, session.clone());

        Ok((session, answer_sdp))
    }
    pub async fn delete_session(&self, session_id: &ClientSessionId) -> anyhow::Result<()> {
        if let Some(session) = self.sessions.remove(session_id) {
            session.1.conn.close().await?;
        }
        Ok(())
    }
}

pub struct WebRtcSession {
    track: Arc<TrackLocalStaticRTP>,
    conn: RTCPeerConnection,
    session_id: ClientSessionId,
    source_id: VideoSourceId,
}

impl WebRtcSession {
    pub fn new(
        source_id: VideoSourceId,
        track: Arc<TrackLocalStaticRTP>,
        session_id: ClientSessionId,
        conn: RTCPeerConnection,
    ) -> Self {
        Self {
            track,
            conn,
            session_id,
            source_id,
        }
    }

    pub fn session_id(&self) -> &Uuid {
        &self.session_id
    }
}

#[async_trait]
impl RtpConsumer for WebRtcSession {
    fn id(&self) -> &ClientSessionId {
        self.session_id()
    }

    async fn on_new_packet(&self, frame: Arc<RtpPacket>) {
        log::trace!(
            "RTP: pt={}, marker={}, seq={}, ts={}, payload_len={}",
            frame.header.payload_type,
            frame.header.marker,
            frame.header.seq,
            frame.header.timestamp,
            frame.payload.len()
        );

        // Serialize frame to bytes for WebRTC track
        let packet = frame.to_bytes();
        if let Err(err) = self.track.write(&packet).await {
            log::error!("Failed to send RTP packet: {}", err);
        }
    }
}

impl RtpVideoPublisher for WebRtcSession {
    fn source_id(&self) -> &VideoSourceId {
        &self.source_id
    }
}
