use crate::app::{ClientSessionId, VideoSourceId};
use crate::common::rtp::RtpPacket;
use crate::common::traits::{RtpConsumer, RtpVideoPublisher};
use crate::common::VideoCodec;
use crate::utils::get_current_unix_timestamp;
use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocket};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use media_server_api_models::{WscRtpClientMessage, WscRtpServerMessage};
use parking_lot::Mutex;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::common::rtp::CodecParameters;
use sdp::description::common::{Address, ConnectionInformation};
use sdp::description::media::{MediaDescription, MediaName, RangedPort};
use sdp::description::session::{Origin, SessionDescription, TimeDescription, Timing};

pub fn build_unicast_sdp(
    source_id: &VideoSourceId,
    codec: &VideoCodec,
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
    // Use a hash of the source_id string for the numeric session_id in SDP
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

    let codec_name = match codec {
        VideoCodec::H264 => "H264",
        VideoCodec::H265 => "H265",
    };
    let fmtp = params.fmtp(codec);

    let mut media = MediaDescription {
        media_name: MediaName {
            media: "video".to_string(),
            port: RangedPort {
                value: client_port as isize,
                range: None,
            },
            protos: vec!["RTP".to_string(), "AVP".to_string()],
            formats: Vec::new(),
        },
        ..Default::default()
    };
    media = media
        .with_codec(96, codec_name.to_string(), 90000, 0, fmtp)
        .with_property_attribute("sendonly".to_string());
    session.media_descriptions.push(media);
    session.marshal()
}

const UDP_HOLEPUNCH_HEADER: &str = "t5rtp";
const PORT_RANGE_START: u16 = 30000;
const PORT_RANGE_END: u16 = 40000;

pub struct WscRtpPublisher {
    id: ClientSessionId,
    video_source_id: VideoSourceId,
    dest_sock: Mutex<Option<Arc<UdpSocket>>>,
    holepunch_listener: Mutex<Option<UdpSocket>>,
    holepunch_port: Mutex<Option<u16>>,
    dead: std::sync::atomic::AtomicBool,
}

impl WscRtpPublisher {
    pub fn new(video_source_id: VideoSourceId) -> Self {
        Self {
            id: Uuid::new_v4(),
            video_source_id,
            dest_sock: Mutex::new(None),
            holepunch_listener: Mutex::new(None),
            holepunch_port: Mutex::new(None),
            dead: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Allocate a UDP listener port in the range 30000-40000 and start listening for hole punch
    pub async fn start_holepunch_listener(self: &Arc<Self>) -> anyhow::Result<u16> {
        // Try to bind to a port in the range
        for port in PORT_RANGE_START..=PORT_RANGE_END {
            match UdpSocket::bind(format!("0.0.0.0:{}", port)).await {
                Ok(socket) => {
                    log::debug!("Session {} allocated holepunch port {}", self.id, port);
                    *self.holepunch_port.lock() = Some(port);
                    *self.holepunch_listener.lock() = Some(socket);

                    // Spawn the listener task
                    let self_clone = Arc::clone(self);
                    tokio::spawn(async move {
                        self_clone.run_holepunch_listener().await;
                    });

                    return Ok(port);
                }
                Err(_) => continue,
            }
        }

        anyhow::bail!(
            "No available ports in range {}-{}",
            PORT_RANGE_START,
            PORT_RANGE_END
        )
    }

    async fn run_holepunch_listener(&self) {
        // Take ownership of the socket
        let socket = {
            let mut guard = self.holepunch_listener.lock();
            match guard.take() {
                Some(sock) => sock,
                None => return,
            }
        };

        let mut buf = [0u8; 1024];
        loop {
            let res = socket.recv_from(&mut buf).await;
            if let Err(err) = res {
                log::error!("Session {}: Failed to receive UDP packet: {}", self.id, err);
                break;
            }

            let (len, src) = res.unwrap();
            if len == 0 {
                continue;
            }

            let payload = match std::str::from_utf8(&buf[..len]) {
                Ok(value) => value,
                Err(_) => continue,
            };

            log::debug!(
                "Session {}: got holepunch payload {:?} from {:?}",
                self.id,
                payload,
                src
            );

            let mut parts = payload.split_whitespace();
            if let Some(header_maybe) = parts.next() {
                if header_maybe == UDP_HOLEPUNCH_HEADER {
                    if let Some(Ok(session_id)) = parts.next().map(Uuid::parse_str) {
                        if session_id != self.id {
                            log::warn!(
                                "Session {}: received holepunch for wrong session {}",
                                self.id,
                                session_id
                            );
                            continue;
                        }

                        // Parse optional client port (third parameter in hole punch packet)
                        let client_port = parts.next().and_then(|p| p.parse::<u16>().ok());

                        // Determine the actual destination address to send packets to.
                        // For local/private networks (localhost, LAN, link-local):
                        //   Use the client's actual listening port if provided, since there's no NAT
                        //   and the client is listening on a different port than what we received from.
                        // For public IPs through NAT:
                        //   Use the NAT-translated address/port (src) since that's the hole punched port.
                        let dest_addr = if should_override_port(src.ip()) {
                            match client_port {
                                Some(port) => SocketAddr::new(src.ip(), port),
                                None => {
                                    log::error!(
                                        "Session {}: local connection must send dest port",
                                        self.id
                                    );
                                    continue;
                                }
                            }
                        } else {
                            src
                        };

                        // Bind a new socket and connect it to the destination address
                        match UdpSocket::bind("0.0.0.0:0").await {
                            Ok(dst_sock) => {
                                // Connect the socket to the destination so we can use send() instead of send_to()
                                if let Err(err) = dst_sock.connect(dest_addr).await {
                                    log::error!(
                                        "Session {}: Failed to connect UDP socket to {}: {}",
                                        self.id,
                                        dest_addr,
                                        err
                                    );
                                    continue;
                                }

                                log::info!(
                                    "Session {}: UDP destination set to {}",
                                    self.id,
                                    dest_addr
                                );
                                *self.dest_sock.lock() = Some(Arc::new(dst_sock));

                                // We got what we need, we can stop listening
                                break;
                            }
                            Err(err) => {
                                log::error!(
                                    "Session {}: Failed to bind UDP socket: {}",
                                    self.id,
                                    err
                                );
                            }
                        }
                    }
                }
            }
        }

        log::debug!("Session {}: holepunch listener stopped", self.id);
    }

    pub fn get_holepunch_port(&self) -> Option<u16> {
        *self.holepunch_port.lock()
    }

    pub fn get_sdp(&self, codec: &VideoCodec, codec_params: &CodecParameters) -> Option<String> {
        let addr = {
            let sock_guard = self.dest_sock.lock();
            match sock_guard.as_ref() {
                Some(sock) => sock.peer_addr().ok()?,
                None => return None,
            }
        };
        Some(build_unicast_sdp(
            &self.video_source_id,
            codec,
            addr,
            codec_params,
        ))
    }

    pub fn shutdown(&self) {
        self.dead.store(true, Ordering::Relaxed);
        // Clear the sockets to close them
        *self.dest_sock.lock() = None;
        *self.holepunch_listener.lock() = None;
    }
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
        let dest_sock = {
            let dest_sock_guard = self.dest_sock.lock();
            match dest_sock_guard.as_ref() {
                Some(dest_sock) => dest_sock.clone(),
                None => {
                    return;
                }
            }
        };
        if let Err(err) = dest_sock.send(&packet.to_bytes()).await {
            log::warn!("Session {}: failed to send RTP packet: {}", self.id, err);
        }
    }
}

impl RtpVideoPublisher for WscRtpPublisher {
    fn source_id(&self) -> &VideoSourceId {
        &self.video_source_id
    }
}

fn should_override_port(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(addr) => addr.is_loopback() || addr.is_private() || addr.is_link_local(),
        IpAddr::V6(addr) => {
            addr.is_loopback() || addr.is_unique_local() || addr.is_unicast_link_local()
        }
    }
}

// WebSocket handling
pub type WsSender = SplitSink<WebSocket, Message>;

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
    get_sdp: impl Fn() -> Option<String>,
) {
    let session_id = *publisher.id();
    let source_id = publisher.source_id().clone();

    let (mut sender, mut receiver) = ws.split();

    let mut holepunch_port = None;
    while holepunch_port.is_none() {
        tokio::select! {
            port_result = publisher.start_holepunch_listener() => {
                match port_result {
                    Ok(port) => holepunch_port = Some(port),
                    Err(err) => {
                        log::error!(
                            "Session {}: failed to allocate holepunch port: {}",
                            session_id,
                            err
                        );
                        let _ = send_wsc_rtp_message(
                            &mut sender,
                            WscRtpServerMessage::Error {
                                message: format!("Failed to allocate port: {}", err),
                            },
                        )
                        .await;
                        publisher.shutdown();
                        return;
                    }
                }
            }
            // make sure that we don't hang for ever waiting for an holepunch
            msg = receiver.next() => {
                if let Some(msg) = msg {
                    // if websocket disconnects, we need to stop the holepunch listener
                    match msg {
                        Err(err) => {
                            log::info!("Session {}: websocket disconnected: {}", session_id, err);
                            publisher.shutdown();
                            return;
                        }
                        _ =>{}
                    }
                }
            }
        }
    }
    // Start the holepunch listener and get the allocated port
    
    let holepunch_port = match publisher.start_holepunch_listener().await {
        Ok(port) => port,
        Err(err) => {
            log::error!(
                "Session {}: failed to allocate holepunch port: {}",
                session_id,
                err
            );
            let _ = send_wsc_rtp_message(
                &mut sender,
                WscRtpServerMessage::Error {
                    message: format!("Failed to allocate port: {}", err),
                },
            )
            .await;
            publisher.shutdown();
            return;
        }
    };
    let last_ping = Arc::new(AtomicU64::new(get_current_unix_timestamp()));
    let (ping_timeout_tx, mut ping_timeout_rx) = tokio::sync::oneshot::channel();

    let _last_ping_checker_task = tokio::spawn(last_ping_checker(
        last_ping.clone(),
        ping_timeout_tx,
        session_id,
    ));

    // Send init message with the allocated holepunch port
    if send_wsc_rtp_message(
        &mut sender,
        WscRtpServerMessage::Init {
            token: session_id.to_string(),
            holepunch_port,
        },
    )
    .await
    .is_err()
    {
        publisher.shutdown();
        return;
    }

    log::info!(
        "Session {}: started for stream {} on port {}",
        session_id,
        source_id,
        holepunch_port
    );

    let mut last_sdp = None;

    loop {
        tokio::select! {
            _ = &mut ping_timeout_rx => {
                log::warn!("Session {}: closed due to ping timeout", session_id);
                break;
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                if let Some(sdp) = get_sdp() {
                    let new_sdp = Some(sdp.clone());
                    if last_sdp != new_sdp {
                        if send_wsc_rtp_message(&mut sender, WscRtpServerMessage::Sdp { sdp }).await.is_ok() {
                            last_sdp = new_sdp;
                        }
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
                    None => break,
                };

                let data: Vec<u8> = match msg {
                    Message::Text(text) => text.into_bytes(),
                    Message::Binary(data) => data.to_vec(),
                    Message::Ping(payload) => {
                        last_ping.store(get_current_unix_timestamp(), Ordering::Relaxed);
                        if sender.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                        continue;
                    },
                    Message::Pong(_) => {
                        last_ping.store(get_current_unix_timestamp(), Ordering::Relaxed);
                        continue;
                    },
                    Message::Close(_) => break,
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
