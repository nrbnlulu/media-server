use crate::app::VideoSourceId;
use crate::common::VideoCodec;
use crate::common::rtp::RtpPacket;
use crate::common::traits::{RtpConsumer, RtpVideoPublisher, VideoSource};
use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use media_server_api_models::{ClientSessionId, PlaybackEvent, SessionMode, wsc_rtp as proto};
use media_server_api_models::{WscRtpClientMessage, WscRtpServerMessage};
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::net::UdpSocket;
use uuid::Uuid;

use crate::common::rtp::CodecParameters;
use sdp::description::common::{Address, ConnectionInformation};
use sdp::description::media::{MediaDescription, MediaName, RangedPort};
use sdp::description::session::{Origin, SessionDescription, TimeDescription, Timing};

/// Stitch at the transport boundary so discarded queue entries do not create
/// artificial sequence gaps or carry an obsolete playback clock onto the wire.
struct WireTimeline {
    stitcher: crate::common::rtp::RtpStitcher,
    generation: Option<u64>,
}
impl WireTimeline {
    fn new() -> Self {
        Self {
            stitcher: crate::common::rtp::RtpStitcher::new(rand::random()),
            generation: None,
        }
    }
    fn rewrite(&mut self, bytes: &mut [u8], generation: u64) {
        let Some(header) = crate::common::rtp::RtpHeader::from_bytes(bytes) else {
            return;
        };
        if self.generation != Some(generation) {
            self.stitcher.trigger_source_switch();
            self.generation = Some(generation);
        }
        let rewritten = self.stitcher.stitch(&RtpPacket::new(header, Arc::from([])));
        bytes[..12].copy_from_slice(&rewritten.header.to_bytes());
    }
}
type QueuedRtp = (u64, Vec<u8>);

struct Outbound {
    message: Message,
    generation: Option<u64>,
}
type WsSender = tokio::sync::mpsc::Sender<Outbound>;
struct ConnectionTasks(Vec<tokio::task::AbortHandle>);
impl Drop for ConnectionTasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}
async fn enqueue(
    sender: &WsSender,
    message: Message,
    generation: Option<u64>,
) -> anyhow::Result<()> {
    sender
        .send(Outbound {
            message,
            generation,
        })
        .await
        .map_err(|_| anyhow::anyhow!("WebSocket writer closed"))
}

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
    let mut session = SessionDescription {
        version: 0,
        ..Default::default()
    };
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

enum SessionControl {
    Mode(SessionMode),
    Playback(PlaybackEvent),
    Error(String),
}

pub struct WscRtpPublisher {
    id: ClientSessionId,
    video_source_id: VideoSourceId,
    /// Channel used to deliver RTP bytes to the active transport (UDP or WebSocket).
    rtp_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<QueuedRtp>>>,
    /// Channel used to deliver SDP updates when codec parameters change.
    sdp_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>,
    /// Channel used to deliver session mode changes to the WebSocket main loop.
    control_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<(u64, SessionControl)>>>,
    /// Cached current SDP so it's available even if update_params was called before the channel was set up.
    current_sdp: Mutex<Option<String>>,
    playback: Mutex<Option<PlaybackEvent>>,
    playback_events: std::sync::atomic::AtomicBool,
    generation: AtomicU64,
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
            control_tx: Mutex::new(None),
            current_sdp: Mutex::new(None),
            playback: Mutex::new(None),
            playback_events: std::sync::atomic::AtomicBool::new(false),
            generation: AtomicU64::new(0),
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
        *self.control_tx.lock() = None;
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
        if let Some(tx) = tx
            && tx
                .send((self.generation.load(Ordering::Acquire), packet.to_bytes()))
                .is_err()
        {
            log::warn!("Session {}: RTP channel closed", self.id);
        }
    }

    async fn on_playback_event(&self, event: PlaybackEvent) {
        *self.playback.lock() = Some(event.clone());
        let control = if self.playback_events.load(Ordering::Acquire) {
            Some(SessionControl::Playback(event))
        } else {
            match event {
                PlaybackEvent::Error { message, .. }
                | PlaybackEvent::RenegotiationRequired { message, .. } => {
                    Some(SessionControl::Error(message))
                }
                _ => None,
            }
        };
        if let Some(control) = control
            && let Some(tx) = self.control_tx.lock().as_ref()
        {
            let _ = tx.send((self.generation.load(Ordering::Acquire), control));
        }
    }
    fn playback_event(&self) -> Option<PlaybackEvent> {
        self.playback.lock().clone()
    }
    fn on_source_switch(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
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

#[async_trait]
impl RtpVideoPublisher for WscRtpPublisher {
    fn source_id(&self) -> &VideoSourceId {
        &self.video_source_id
    }

    async fn on_session_mode_change(&self, mode: SessionMode) {
        if matches!(mode, SessionMode::Live) {
            *self.playback.lock() = None;
        }
        if let Some(tx) = self.control_tx.lock().as_ref()
            && tx
                .send((
                    self.generation.load(Ordering::Acquire),
                    SessionControl::Mode(mode),
                ))
                .is_err()
        {
            log::warn!(
                "Session {}: failed to send mode change, channel closed",
                self.id
            );
        }
    }
}

async fn handle_wsc_rtp_message(
    session_id: &ClientSessionId,
    sender: &mut WsSender,
    payload: &[u8],
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
    enqueue(sender, Message::Text(payload), None).await?;
    Ok(())
}

async fn receive_messages<S, E>(
    mut receiver: S,
    mut sender: WsSender,
    session_id: ClientSessionId,
    timeout: Duration,
) -> anyhow::Result<()>
where
    S: futures::Stream<Item = Result<Message, E>> + Unpin,
    E: std::fmt::Display,
{
    loop {
        // Tokio deadlines use a monotonic clock and apply from connection startup.
        let message = tokio::time::timeout(timeout, receiver.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("WebSocket EOF"))?
            .map_err(|error| anyhow::anyhow!("WebSocket receive failed: {error}"))?;
        match message {
            Message::Ping(payload) => enqueue(&sender, Message::Pong(payload), None).await?,
            Message::Pong(_) => {}
            Message::Close(_) => return Ok(()),
            Message::Text(text) => {
                handle_wsc_rtp_message(&session_id, &mut sender, text.as_bytes()).await?
            }
            Message::Binary(bytes) => {
                handle_wsc_rtp_message(&session_id, &mut sender, &bytes).await?
            }
        }
    }
}

async fn write_messages<S>(
    mut wire_sender: S,
    mut outgoing: tokio::sync::mpsc::Receiver<Outbound>,
    publisher: Arc<WscRtpPublisher>,
) where
    S: futures::Sink<Message> + Unpin,
{
    let mut timeline = WireTimeline::new();
    while let Some(mut output) = outgoing.recv().await {
        if output
            .generation
            .is_some_and(|generation| generation != publisher.generation.load(Ordering::Acquire))
        {
            continue;
        }
        if let (Some(generation), Message::Binary(bytes)) = (output.generation, &mut output.message)
        {
            timeline.rewrite(bytes, generation);
        }
        match tokio::time::timeout(Duration::from_secs(5), wire_sender.send(output.message)).await {
            Ok(Ok(())) => {}
            _ => break,
        }
    }
}

pub async fn handle_incoming_wsc_rtp_websocket(
    ws: WebSocket,
    publisher: Arc<WscRtpPublisher>,
    source: Arc<dyn VideoSource>,
    force_websocket_transport: bool,
    playback_events: bool,
) {
    publisher
        .playback_events
        .store(playback_events, Ordering::Release);
    let session_id = *publisher.id();
    let source_id = publisher.source_id().clone();

    let (wire_sender, receiver) = ws.split();
    let (mut sender, outgoing) = tokio::sync::mpsc::channel::<Outbound>(64);
    let mut writer = tokio::spawn(write_messages(wire_sender, outgoing, publisher.clone()));
    let mut reader = tokio::spawn(receive_messages(
        receiver,
        sender.clone(),
        session_id,
        Duration::from_secs(5),
    ));
    let _tasks = ConnectionTasks(vec![writer.abort_handle(), reader.abort_handle()]);

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

    if let Err(ref err) = udp_port_result
        && !force_websocket_transport
    {
        log::warn!(
            "Session {}: failed to allocate holepunch port ({}), will use WebSocket transport",
            session_id,
            err
        );
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
    let mut last_sdp = None;
    if let Some(sdp) = publisher.get_sdp() {
        log::info!("Session {}: sending cached SDP", session_id);
        last_sdp = Some(sdp.clone());
        let _ = send_wsc_rtp_message(&mut sender, WscRtpServerMessage::Sdp { sdp }).await;
    } else {
        let initial = tokio::select! {
            _ = &mut reader => { publisher.shutdown(); return; },
            _ = &mut writer => { publisher.shutdown(); return; },
            value = tokio::time::timeout(Duration::from_secs(5), sdp_rx.recv()) => value,
        };
        match initial {
            Ok(Some(sdp)) => {
                log::info!("Session {}: sending SDP", session_id);
                last_sdp = Some(sdp.clone());
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
    // The independent receiver continues handling pings during negotiation.
    let udp_sock = if let Ok((_, rcv_sock)) = udp_port_result {
        tokio::select! {
            _ = &mut reader => { publisher.shutdown(); return; },
            _ = &mut writer => { publisher.shutdown(); return; },
            result = negotiate_with_timeout(&publisher, rcv_sock, Duration::from_secs(5), &session_id) => result.ok(),
        }
    } else {
        None
    };

    // Create the RTP channel.
    let (rtp_tx, mut rtp_rx) = tokio::sync::mpsc::unbounded_channel::<QueuedRtp>();
    *publisher.rtp_tx.lock() = Some(rtp_tx);

    // Create the control channel for session mode changes.
    let (control_tx, mut control_rx) =
        tokio::sync::mpsc::unbounded_channel::<(u64, SessionControl)>();
    *publisher.control_tx.lock() = Some(control_tx);

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

    let mut udp_timeline = WireTimeline::new();
    loop {
        tokio::select! {
            biased;
            _ = &mut reader => break,
            _ = &mut writer => break,
            Some(sdp) = sdp_rx.recv() => {
                if last_sdp.as_ref() != Some(&sdp) {
                    log::info!("Session {}: sending SDP update", session_id);
                    if send_wsc_rtp_message(&mut sender, WscRtpServerMessage::Sdp { sdp: sdp.clone() }).await.is_ok() {
                        last_sdp = Some(sdp);
                    }
                }
            }
            Some((generation, mode)) = control_rx.recv() => {
                if generation != publisher.generation.load(Ordering::Acquire) { continue; }
                let message = match mode { SessionControl::Mode(mode) => WscRtpServerMessage::SessionMode(mode), SessionControl::Playback(event) => WscRtpServerMessage::Playback { event }, SessionControl::Error(message) => WscRtpServerMessage::Error { message } };
                if let Ok(payload) = serde_json::to_string(&message) {
                    let _ = enqueue(&sender, Message::Text(payload), Some(generation)).await;
                }
            }
            Some((generation, mut rtp_bytes)) = rtp_rx.recv() => {
                if generation != publisher.generation.load(Ordering::Acquire) { continue; }
                if let Some(ref sock) = udp_sock {
                    udp_timeline.rewrite(&mut rtp_bytes, generation);
                    match tokio::time::timeout(Duration::from_secs(5), sock.send(&rtp_bytes)).await {
                        Ok(Ok(_)) => {},
                        _ => { log::warn!("Session {}: UDP send failed or timed out", session_id); break; }
                    }
                } else if enqueue(&sender, Message::Binary(rtp_bytes), Some(generation)).await.is_err() { break; }
            }
        }
    }

    log::info!("Session {}: closed for stream {}", session_id, source_id);
    publisher.shutdown();
}

#[cfg(test)]
mod transition_tests {
    use super::*;
    use crate::common::rtp::RtpHeader;

    #[tokio::test]
    async fn queued_media_retains_its_generation_across_a_switch() {
        let publisher = WscRtpPublisher::new("test".into());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        *publisher.rtp_tx.lock() = Some(tx);
        let packet = Arc::new(RtpPacket::new(
            RtpHeader {
                payload_type: 96,
                marker: true,
                seq: 1,
                timestamp: 100,
                ssrc: 4,
            },
            Arc::from([0x65u8]),
        ));
        publisher.on_new_packet(packet.clone()).await;
        publisher.on_source_switch();
        publisher.on_new_packet(packet).await;
        let (old, _) = rx.recv().await.unwrap();
        let (current, _) = rx.recv().await.unwrap();
        assert_ne!(old, publisher.generation.load(Ordering::Acquire));
        assert_eq!(current, publisher.generation.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn writer_discards_obsolete_queue_entries_before_rewriting_rtp() {
        let publisher = Arc::new(WscRtpPublisher::new("test".into()));
        let (wire, mut received) = futures::channel::mpsc::unbounded::<Message>();
        let (out, queue) = tokio::sync::mpsc::channel(8);
        let writer = tokio::spawn(write_messages(wire, queue, publisher.clone()));
        let packet = |seq, timestamp| {
            Message::Binary(
                RtpPacket::new(
                    RtpHeader {
                        payload_type: 96,
                        marker: true,
                        seq,
                        timestamp,
                        ssrc: 99,
                    },
                    Arc::from([0x65u8]),
                )
                .to_bytes(),
            )
        };
        enqueue(&out, packet(10, 90000), Some(0)).await.unwrap();
        let Message::Binary(first) = received.next().await.unwrap() else {
            panic!("expected RTP");
        };
        publisher.on_source_switch();
        enqueue(&out, packet(200, 900000), Some(0)).await.unwrap();
        enqueue(&out, packet(500, 3000000), Some(1)).await.unwrap();
        drop(out);
        writer.await.unwrap();
        let Message::Binary(next) = received.next().await.unwrap() else {
            panic!("expected RTP");
        };
        assert!(received.next().await.is_none());
        let first = RtpHeader::from_bytes(&first).unwrap();
        let next = RtpHeader::from_bytes(&next).unwrap();
        assert_eq!(next.seq, first.seq.wrapping_add(1));
        assert_eq!(next.timestamp, first.timestamp.wrapping_add(3000));
        assert_eq!(next.ssrc, first.ssrc);
    }

    #[tokio::test]
    async fn playback_events_are_opt_in_but_fatal_errors_remain_compatible() {
        let publisher = WscRtpPublisher::new("test".into());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        *publisher.control_tx.lock() = Some(tx);
        publisher
            .on_playback_event(PlaybackEvent::Waiting { timestamp: 123 })
            .await;
        assert!(rx.try_recv().is_err());
        publisher
            .on_playback_event(PlaybackEvent::Error {
                timestamp: 123,
                message: "read failed".into(),
            })
            .await;
        assert!(
            matches!(rx.recv().await.unwrap().1, SessionControl::Error(message) if message == "read failed")
        );
        publisher.playback_events.store(true, Ordering::Release);
        let event = PlaybackEvent::Ended { timestamp: 123 };
        publisher.on_playback_event(event.clone()).await;
        assert!(
            matches!(rx.recv().await.unwrap().1, SessionControl::Playback(actual) if actual == event)
        );
        assert_eq!(publisher.playback_event(), Some(event));
    }
    #[test]
    fn playback_event_round_trips_and_old_query_defaults_remain_valid() {
        let query: media_server_api_models::WscRtpQuery = serde_json::from_str("{}").unwrap();
        assert!(!query.playback_events);
        let message = WscRtpServerMessage::Playback {
            event: PlaybackEvent::GapSkipped { from: 10, to: 20 },
        };
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["type"], "playback");
        assert_eq!(json["event"]["state"], "gap_skipped");
        assert!(matches!(
            serde_json::from_value::<WscRtpServerMessage>(json).unwrap(),
            WscRtpServerMessage::Playback {
                event: PlaybackEvent::GapSkipped { from: 10, to: 20 }
            }
        ));
    }
    #[test]
    fn wire_timeline_uses_sent_packets_not_discarded_backlog() {
        let mut timeline = WireTimeline::new();
        let packet = |seq, timestamp| {
            RtpPacket::new(
                RtpHeader {
                    payload_type: 96,
                    marker: true,
                    seq,
                    timestamp,
                    ssrc: 123,
                },
                Arc::from([0x65u8]),
            )
            .to_bytes()
        };
        let mut first = packet(10, 90000);
        timeline.rewrite(&mut first, 0);
        // Queued old packets with later timestamps/sequence numbers were discarded.
        let mut live = packet(500, 5000000);
        timeline.rewrite(&mut live, 1);
        let first = RtpHeader::from_bytes(&first).unwrap();
        let live = RtpHeader::from_bytes(&live).unwrap();
        assert_eq!(live.seq, first.seq.wrapping_add(1));
        assert_eq!(live.timestamp, first.timestamp.wrapping_add(3000));
        assert_eq!(live.ssrc, first.ssrc);
    }
    #[tokio::test]
    async fn heartbeat_is_answered_while_negotiation_is_still_pending() {
        let (mut tx, rx) = futures::channel::mpsc::unbounded::<Result<Message, std::io::Error>>();
        let (out, mut incoming) = tokio::sync::mpsc::channel(8);
        let reader = tokio::spawn(receive_messages(
            rx,
            out,
            Uuid::new_v4(),
            Duration::from_millis(150),
        ));
        let startup = tokio::time::sleep(Duration::from_millis(500));
        tokio::pin!(startup);
        for index in 0..6 {
            let message = if index % 2 == 0 {
                Message::Text(serde_json::to_string(&WscRtpClientMessage::Ping).unwrap())
            } else {
                Message::Ping(vec![index])
            };
            tx.send(Ok(message)).await.unwrap();
            let response = tokio::time::timeout(Duration::from_millis(100), incoming.recv())
                .await
                .unwrap()
                .unwrap();
            match response.message {
                Message::Text(text) => assert!(matches!(
                    serde_json::from_str::<WscRtpServerMessage>(&text).unwrap(),
                    WscRtpServerMessage::Pong
                )),
                Message::Pong(payload) => assert_eq!(payload, vec![index]),
                _ => panic!("unexpected heartbeat response"),
            }
            assert!(!reader.is_finished());
            tokio::time::sleep(Duration::from_millis(35)).await;
        }
        assert!(
            tokio::time::timeout(Duration::ZERO, &mut startup)
                .await
                .is_err()
        );
        tx.send(Ok(Message::Close(None))).await.unwrap();
        reader.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn heartbeat_timeout_and_eof_end_the_receiver() {
        let (tx, rx) = futures::channel::mpsc::unbounded::<Result<Message, std::io::Error>>();
        let (out, _incoming) = tokio::sync::mpsc::channel(8);
        assert!(
            receive_messages(rx, out.clone(), Uuid::new_v4(), Duration::from_millis(30))
                .await
                .is_err()
        );
        drop(tx);
        let (tx, rx) = futures::channel::mpsc::unbounded::<Result<Message, std::io::Error>>();
        drop(tx);
        let ended = tokio::time::timeout(
            Duration::from_millis(50),
            receive_messages(rx, out, Uuid::new_v4(), Duration::from_secs(5)),
        )
        .await;
        assert!(ended.unwrap().is_err());
    }
}
