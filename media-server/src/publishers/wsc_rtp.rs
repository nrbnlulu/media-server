use crate::app::VideoSourceId;
use crate::common::VideoCodec;
use crate::common::rtp::RtpPacket;
use crate::common::traits::{RtpConsumer, RtpVideoPublisher, VideoSource};
use crate::utils::get_current_unix_timestamp;
use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocket};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use media_server_api_models::{ClientSessionId, VideoSourceInput, wsc_rtp as proto};
use media_server_api_models::{WscRtpClientMessage, WscRtpServerMessage};
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::common::rtp::CodecParameters;
use sdp::description::common::{Address, ConnectionInformation};
use sdp::description::media::{MediaDescription, MediaName, RangedPort};
use sdp::description::session::{Origin, SessionDescription, TimeDescription, Timing};

pub type WsSender = SplitSink<WebSocket, Message>;

pub fn build_unicast_sdp(
    source_id: &VideoSourceId,
    addr: SocketAddr,
    params: &CodecParameters,
) -> String {
    let (address_type, ip_address) = match addr.ip() {
        std::net::IpAddr::V4(_) => ("IP4", std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
        std::net::IpAddr::V6(_) => ("IP6", std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)),
    };
    let client_port = addr.port();
    let mut session = SessionDescription::default();
    session.version = 0;
    let session_id_hash = source_id
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    session.origin = Origin {
        username: "-".to_string(),
        session_id: session_id_hash,
        session_version: 0,
        network_type: "IN".to_string(),
        address_type: address_type.to_string(),
        unicast_address: ip_address.to_string(),
    };
    session.session_name = "media-server".to_string();
    session.connection_information = Some(ConnectionInformation {
        network_type: "IN".to_string(),
        address_type: address_type.to_string(),
        address: Some(Address {
            address: ip_address.to_string(),
            ttl: None,
            range: None,
        }),
    });
    session.time_descriptions.push(TimeDescription {
        timing: Timing {
            start_time: 0,
            stop_time: 0,
        },
        repeat_times: Vec::new(),
    });

    let codec = params.codec();
    let codec_name = match codec {
        VideoCodec::H264 => "H264",
        VideoCodec::H265 => "H265",
    };
    let fmtp = params.fmtp();

    let mut media = MediaDescription {
        media_name: MediaName {
            media: "video".to_string(),
            port: RangedPort {
                value: client_port as isize,
                range: None,
            },
            // AVP is for audio, though we currently support only video
            protos: vec!["RTP".to_string(), "AVP".to_string()],
            formats: Vec::new(),
        },
        ..Default::default()
    };
    media = media
        .with_codec(96, codec_name.to_string(), 90000, 0, fmtp)
        .with_property_attribute("sendonly".to_string());
    session.media_descriptions.push(media);
    session.marshal().replace("\r\n", "\n")
}

pub struct WscRtpPublisher {
    id: ClientSessionId,
    video_source_id: VideoSourceId,
    /// Channel used to deliver RTP bytes to the active transport (UDP or WebSocket).
    rtp_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>,
    /// Channel used to deliver SDP updates when codec parameters change.
    sdp_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>,
    /// Cached current SDP so it's available even if update_params was called before the channel was set up.
    current_sdp: Mutex<Option<String>>,
    holepunch_port: Mutex<Option<u16>>,
    dead: std::sync::atomic::AtomicBool,
}

impl WscRtpPublisher {
    pub fn new(video_source_id: VideoSourceId) -> Self {
        Self {
            id: Uuid::new_v4(),
            video_source_id,
            rtp_tx: Mutex::new(None),
            sdp_tx: Mutex::new(None),
            current_sdp: Mutex::new(None),
            holepunch_port: Mutex::new(None),
            dead: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Allocate a port in the range 30000-40000 and bind a UDP listener on it.
    pub async fn bind_holepunch_port(&self) -> anyhow::Result<(u16, UdpSocket)> {
        for port in proto::PORT_RANGE_START..=proto::PORT_RANGE_END {
            match UdpSocket::bind(format!("0.0.0.0:{}", port)).await {
                Ok(udp_sock) => {
                    log::debug!("Session {} allocated UDP port {}", self.id, port);
                    *self.holepunch_port.lock() = Some(port);
                    return Ok((port, udp_sock));
                }
                Err(_) => continue,
            }
        }

        anyhow::bail!(
            "No available ports in range {}-{}",
            proto::PORT_RANGE_START,
            proto::PORT_RANGE_END
        )
    }

    /// Wait for a UDP hole-punch, confirm with dummy/ack, and return a connected UDP socket.
    /// Returns `None` if no ack received (caller should fall back to WebSocket transport).
    async fn negotiate_udp(&self, udp_socket: UdpSocket) -> anyhow::Result<Option<Arc<UdpSocket>>> {
        let mut buf = [0u8; 1024];

        loop {
            let (len, src) = udp_socket.recv_from(&mut buf).await?;

            let payload = match std::str::from_utf8(&buf[..len]) {
                Ok(s) => s,
                Err(_) => continue,
            };

            log::info!(
                "Session {}: UDP holepunch from {}: {:?}",
                self.id,
                src,
                payload
            );

            if !is_valid_holepunch(&self.id, payload) {
                continue;
            }

            let dest_sock = match UdpSocket::bind("0.0.0.0:0").await {
                Ok(s) => s,
                Err(err) => {
                    log::error!(
                        "Session {}: failed to bind UDP dest socket: {}",
                        self.id,
                        err
                    );
                    continue;
                }
            };
            if let Err(err) = dest_sock.connect(src).await {
                log::error!(
                    "Session {}: failed to connect UDP socket to {}: {}",
                    self.id,
                    src,
                    err
                );
                continue;
            }

            if send_dummy_and_wait_ack(&self.id, &dest_sock, &udp_socket).await {
                log::info!("Session {}: UDP transport confirmed", self.id);
                return Ok(Some(Arc::new(dest_sock)));
            } else {
                log::info!(
                    "Session {}: no UDP ack received, falling back to WebSocket",
                    self.id
                );
                return Ok(None);
            }
        }
    }

    pub fn get_holepunch_port(&self) -> Option<u16> {
        *self.holepunch_port.lock()
    }

    pub fn get_sdp(&self) -> Option<String> {
        self.current_sdp.lock().clone()
    }

    fn build_sdp(&self, codec_params: &CodecParameters) -> Option<String> {
        let addr = "0.0.0.0:0".parse().ok()?;
        Some(build_unicast_sdp(&self.video_source_id, addr, codec_params))
    }

    pub fn shutdown(&self) {
        log::info!("Shutting down WebSocket RTP publisher {}", self.id);
        self.dead.store(true, Ordering::Relaxed);
        *self.rtp_tx.lock() = None;
        *self.sdp_tx.lock() = None;
    }
}

fn is_valid_holepunch(session_id: &Uuid, payload: &str) -> bool {
    let mut parts = payload.split_whitespace();
    matches!(
        (
            parts.next(),
            parts.next().and_then(|s| Uuid::parse_str(s).ok())
        ),
        (Some(h), Some(ref id)) if h == proto::HOLEPUNCH_HEADER && id == session_id
    )
}

/// Send dummy UDP packets and wait for an ack from the client.
/// Returns true if ack received (UDP confirmed working).
async fn send_dummy_and_wait_ack(
    session_id: &Uuid,
    dest_sock: &UdpSocket,
    listen_sock: &UdpSocket,
) -> bool {
    let dummy_msg = format!("{} {}", proto::DUMMY_HEADER, session_id);
    let ack_prefix = format!("{} {}", proto::ACK_HEADER, session_id);
    let mut buf = [0u8; 256];

    for _ in 0..proto::DUMMY_PACKET_COUNT {
        if let Err(err) = dest_sock.send(dummy_msg.as_bytes()).await {
            log::warn!(
                "Session {}: failed to send dummy packet: {}",
                session_id,
                err
            );
        }

        match tokio::time::timeout(
            Duration::from_millis(proto::DUMMY_PACKET_INTERVAL_MS),
            listen_sock.recv_from(&mut buf),
        )
        .await
        {
            Ok(Ok((len, _))) => {
                if std::str::from_utf8(&buf[..len])
                    .map(|s| s.starts_with(&ack_prefix))
                    .unwrap_or(false)
                {
                    return true;
                }
            }
            _ => continue,
        }
    }

    false
}

#[async_trait]
impl RtpConsumer for WscRtpPublisher {
    fn id(&self) -> &ClientSessionId {
        &self.id
    }

    async fn on_new_packet(&self, packet: Arc<RtpPacket>) {
        if self.dead.load(Ordering::Relaxed) {
            return;
        }
        let tx = self.rtp_tx.lock().clone();
        if let Some(tx) = tx {
            if tx.send(packet.to_bytes()).is_err() {
                log::warn!("Session {}: RTP channel closed", self.id);
            }
        }
    }

    fn update_params(&self, params: &CodecParameters) {
        if let Some(sdp) = self.build_sdp(params) {
            *self.current_sdp.lock() = Some(sdp.clone());
            if let Some(tx) = self.sdp_tx.lock().as_ref() {
                let _ = tx.send(sdp);
            }
        }
    }
}

impl RtpVideoPublisher for WscRtpPublisher {
    fn source_id(&self) -> &VideoSourceId {
        &self.video_source_id
    }
}

async fn handle_wsc_rtp_message(
    session_id: &ClientSessionId,
    sender: &mut WsSender,
    payload: &[u8],
    last_ping: &Arc<AtomicU64>,
) -> anyhow::Result<()> {
    let message = match serde_json::from_slice::<WscRtpClientMessage>(payload) {
        Ok(msg) => msg,
        Err(err) => {
            return send_wsc_rtp_message(
                sender,
                WscRtpServerMessage::Error {
                    message: err.to_string(),
                },
            )
            .await;
        }
    };

    match message {
        WscRtpClientMessage::Ping => {
            log::trace!("Session {}: received ping", session_id);
            last_ping.store(get_current_unix_timestamp(), Ordering::Relaxed);
            send_wsc_rtp_message(sender, WscRtpServerMessage::Pong).await
        }
    }
}

async fn send_wsc_rtp_message(
    sender: &mut WsSender,
    message: WscRtpServerMessage,
) -> anyhow::Result<()> {
    let payload = match serde_json::to_string(&message) {
        Ok(payload) => payload,
        Err(err) => {
            log::error!("Failed to serialize wsc-rtp message: {}", err);
            return Err(err.into());
        }
    };
    sender.send(Message::Text(payload)).await?;
    Ok(())
}

async fn last_ping_checker(
    last_ping: Arc<AtomicU64>,
    ping_timeout_tx: oneshot::Sender<()>,
    session_id: ClientSessionId,
) {
    loop {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let current_time = get_current_unix_timestamp();
        let last_ping_time = last_ping.load(Ordering::Relaxed);
        if (current_time - last_ping_time) >= 5000 {
            let _ = ping_timeout_tx.send(());
            log::warn!("Session {}: ping timeout", session_id);
            return;
        }
    }
}

pub async fn handle_incoming_wsc_rtp_websocket(
    ws: WebSocket,
    publisher: Arc<WscRtpPublisher>,
    source: Arc<dyn VideoSource>,
    force_websocket_transport: bool,
) {
    let session_id = *publisher.id();
    let source_id = publisher.source_id().clone();

    let (mut sender, mut receiver) = ws.split();

    let last_ping = Arc::new(AtomicU64::new(get_current_unix_timestamp()));
    let (ping_timeout_tx, mut ping_timeout_rx) = tokio::sync::oneshot::channel();

    let _last_ping_checker_task = tokio::spawn(last_ping_checker(
        last_ping.clone(),
        ping_timeout_tx,
        session_id,
    ));

    // Try to allocate a UDP port. Skip if client requested WebSocket-only transport.
    let udp_port_result = if force_websocket_transport {
        log::info!(
            "Session {}: client requested WebSocket-only transport, skipping UDP",
            session_id
        );
        Err(anyhow::anyhow!("WebSocket-only transport requested"))
    } else {
        publisher.bind_holepunch_port().await
    };

    if let Err(ref err) = udp_port_result {
        if !force_websocket_transport {
            log::warn!(
                "Session {}: failed to allocate holepunch port ({}), will use WebSocket transport",
                session_id,
                err
            );
        }
    }

    let holepunch_port = udp_port_result.as_ref().map(|(p, _)| *p).unwrap_or(0);

    let active_source = source.active_input();
    if send_wsc_rtp_message(
        &mut sender,
        WscRtpServerMessage::Init {
            session_id: session_id.to_string(),
            holepunch_port,
            active_source,
        },
    )
    .await
    .is_err()
    {
        publisher.shutdown();
        return;
    }
    // Create the SDP channel so update_params can push SDP updates.
    let (sdp_tx, mut sdp_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    *publisher.sdp_tx.lock() = Some(sdp_tx);

    // If update_params was already called before the channel existed, the SDP is cached.
    // Use it immediately; otherwise wait up to 5 seconds for the first SDP.
    if let Some(sdp) = publisher.get_sdp() {
        log::info!("Session {}: sending cached SDP", session_id);
        let _ = send_wsc_rtp_message(&mut sender, WscRtpServerMessage::Sdp { sdp }).await;
    } else {
        match tokio::time::timeout(Duration::from_secs(5), sdp_rx.recv()).await {
            Ok(Some(sdp)) => {
                log::info!("Session {}: sending SDP", session_id);
                let _ = send_wsc_rtp_message(&mut sender, WscRtpServerMessage::Sdp { sdp }).await;
            }
            _ => {
                log::warn!("Session {}: timed out waiting for initial SDP", session_id);
            }
        }
    }

    async fn negotiate_with_timeout(
        publisher: &WscRtpPublisher,
        rcv_sock: UdpSocket,
        timeout: Duration,
        session: &ClientSessionId,
    ) -> anyhow::Result<Arc<UdpSocket>> {
        tokio::time::timeout(timeout, publisher.negotiate_udp(rcv_sock))
            .await??
            .ok_or(anyhow::anyhow!(
                "failed to negotiate udp socket for session {}",
                session
            ))
    }

    // Negotiate UDP (5-second timeout). Falls back to WebSocket if unavailable.
    // We do not select! over the receiver here to avoid consuming WebSocket messages
    // (e.g. pings) that should be handled in the main loop.
    let udp_sock = if let Ok((_, rcv_sock)) = udp_port_result {
        negotiate_with_timeout(&publisher, rcv_sock, Duration::from_secs(5), &session_id)
            .await
            .ok()
    } else {
        None
    };

    // Create the RTP channel.
    let (rtp_tx, mut rtp_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    *publisher.rtp_tx.lock() = Some(rtp_tx);

    if udp_sock.is_some() {
        log::info!(
            "Session {}: started on port {} via UDP",
            session_id,
            holepunch_port
        );
    } else {
        log::info!(
            "Session {}: falling back to WebSocket RTP transport",
            session_id
        );
        let _ = send_wsc_rtp_message(&mut sender, WscRtpServerMessage::FallingBackRtpToWs).await;
    }

    let mut last_sdp = None;

    loop {
        tokio::select! {
            _ = &mut ping_timeout_rx => {
                log::warn!("Session {}: closed due to ping timeout", session_id);
                break;
            }
            Some(rtp_bytes) = rtp_rx.recv() => {
                if let Some(ref sock) = udp_sock {
                    if let Err(err) = sock.send(&rtp_bytes).await {
                        log::error!("Session {}: error sending RTP packet: {}", session_id, err);
                    }
                } else {
                    if let Err(err) = sender.send(Message::Binary(rtp_bytes.into())).await {
                        log::error!("Session {}: error sending RTP packet: {}", session_id, err);
                    }
                };
            }
            Some(sdp) = sdp_rx.recv() => {
                if last_sdp.as_ref() != Some(&sdp) {
                    log::info!("Session {}: sending SDP update", session_id);
                    if send_wsc_rtp_message(&mut sender, WscRtpServerMessage::Sdp { sdp: sdp.clone() }).await.is_ok() {
                        last_sdp = Some(sdp);
                    }
                }
            }
            res = receiver.next() => {
                let msg = match res {
                    Some(Ok(msg)) => msg,
                    Some(Err(err)) => {
                        log::error!("Session {}: error receiving WebSocket message: {}", session_id, err);
                        break;
                    }
                    None => continue,
                };

                let data: Vec<u8> = match msg {
                    Message::Text(text) => text.into_bytes(),
                    Message::Binary(data) => data.to_vec(),
                    Message::Ping(payload) => {
                        last_ping.store(get_current_unix_timestamp(), Ordering::Relaxed);
                        if sender.send(Message::Pong(payload)).await.is_err() {
                            log::error!("Session {}: error sending Pong message", session_id);
                        }
                        continue;
                    }
                    Message::Pong(_) => {
                        last_ping.store(get_current_unix_timestamp(), Ordering::Relaxed);
                        continue;
                    }
                    Message::Close(_) => {
                        log::info!("Session {}: error closing WebSocket connection", session_id);
                        break;
                    }
                };

                if let Err(err) = handle_wsc_rtp_message(&session_id, &mut sender, &data, &last_ping).await {
                    log::error!("Session {}: error handling message: {}", session_id, err);
                    break;
                }
            }
        }
    }

    log::info!("Session {}: closed for stream {}", session_id, source_id);
    publisher.shutdown();
}
