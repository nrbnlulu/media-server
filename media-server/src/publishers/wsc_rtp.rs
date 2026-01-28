use crate::app::{AppSettings, ClientSessionId, GlobalState};
use crate::common::rtp::RtpPacket;
use crate::common::traits::{RtpConsumer, RtpVideoPublisher};
use crate::common::VideoCodec;
use crate::utils::get_current_unix_timestamp;
use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocket};
use dashmap::DashMap;
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
    source_id: u64,
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
    session.origin = Origin {
        username: "-".to_string(),
        session_id: source_id,
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
const REGISTRATION_TTL: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct WscRtpRegistrationSnapshot {
    pub destination: Option<SocketAddr>,
    pub client_port: Option<u16>,
}

/// A global listener for hole punching
/// client sends a packet to the server in a certain port
/// and adds a data about the session for the stream it expect to receive
/// in that port.
pub struct WscRtpUdpManager {
    socket: UdpSocket,
    wsc_publishers: DashMap<ClientSessionId, Arc<WscRtpPublisher>>,
}

impl WscRtpUdpManager {
    pub async fn new(app_settings: &AppSettings) -> anyhow::Result<Self> {
        let socket =
            UdpSocket::bind(format!("0.0.0.0:{}", app_settings.wsc_holepunch_port)).await?;
        Ok(Self {
            socket,
            wsc_publishers: DashMap::new(),
        })
    }
    pub fn register_publisher(&self, session_id: ClientSessionId, publisher: Arc<WscRtpPublisher>) {
        self.wsc_publishers.insert(session_id, publisher);
    }
    pub fn remove_publisher(&self, session_id: &ClientSessionId) {
        self.wsc_publishers.remove(session_id);
    }
    pub fn get_sdp_for_session(
        &self,
        session_id: &ClientSessionId,
        codec: &VideoCodec,
        codec_params: &CodecParameters,
    ) -> Option<String> {
        if let Some(publisher) = self.wsc_publishers.get(session_id) {
            Some(publisher.get_sdp(codec, codec_params)?)
        } else {
            None
        }
    }

    pub async fn run(&self) {
        loop {
            let mut buf = [0; 1024];
            let res = self.socket.recv_from(&mut buf).await;
            if let Err(err) = res {
                log::error!("Failed to receive UDP packet: {}", err);
                continue;
            }
            let (len, src) = res.unwrap();
            if len == 0 {
                continue;
            }
            let payload = match std::str::from_utf8(&buf[..len]) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let mut parts = payload.split_whitespace();
            if let Some(header_maybe) = parts.next() {
                if header_maybe == UDP_HOLEPUNCH_HEADER {
                    if let Some(Ok(wsc_rtp_session_id)) = parts.next().map(Uuid::parse_str) {
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
                                    log::error!("a local connection must send dest port");
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
                                // arguably this is better due to kernel caching and isolation.
                                if let Err(err) = dst_sock.connect(dest_addr).await {
                                    log::error!(
                                        "Failed to connect UDP socket to {}: {}",
                                        dest_addr,
                                        err
                                    );
                                    continue;
                                }

                                if let Some(publisher) =
                                    self.wsc_publishers.get(&wsc_rtp_session_id).as_ref()
                                {
                                    publisher.set_socket(dst_sock);
                                } else {
                                    log::error!("Failed to set UDP address for RTP session, make sure first create a session");
                                }
                                continue;
                            }
                            Err(err) => {
                                log::error!("Failed to bind UDP socket: {}", err);
                            }
                        };

                        continue;
                    }
                }
            }
            log::warn!("unable to parse udp holepunch header");
        }
    }
}

pub struct WscRtpPublisher {
    id: ClientSessionId,
    video_source_id: u64,
    dest_sock: Mutex<Option<Arc<UdpSocket>>>,
    dead: std::sync::atomic::AtomicBool,
}

impl WscRtpPublisher {
    pub fn new(video_source_id: u64) -> Self {
        Self {
            id: Uuid::new_v4(),
            video_source_id,
            dest_sock: Mutex::new(None),
            dead: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn set_socket(&self, sock: UdpSocket) {
        *self.dest_sock.lock() = Some(Arc::new(sock));
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
            self.video_source_id,
            codec,
            addr,
            codec_params,
        ))
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
                    log::debug!("dest socket not yet configured");
                    return;
                }
            }
        };
        if let Err(err) = dest_sock.send(&packet.to_bytes()).await {
            // this would usually error out in loopback if there is no actual destination
            // for "real" destinations there is no way we'd know if the packet reached the destination (its UDP after all)
            // so we can't rely on this error for anything useful.
            log::warn!(
                "wsc session {}: send failed: {}",
                self.id,
                err
            );
        }
    }
}

impl RtpVideoPublisher for WscRtpPublisher {
    fn source_id(&self) -> &u64 {
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

// WS stuff
pub type WsSender = SplitSink<WebSocket, Message>;

async fn handle_wsc_rtp_message(
    session_id: &ClientSessionId,
    _app_state: Arc<GlobalState>,
    sender: &mut WsSender,
    _source_id: u64,
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
            log::trace!("wsc received ping session {}", session_id);
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
            log::warn!("Client {} timed out", session_id);
            return;
        }
    }
}

pub async fn handle_incoming_wsc_trp_websocket(
    ws: WebSocket,
    app_state: Arc<GlobalState>,
    source_id: u64,
    client_port: Option<u16>,
    client_addr: SocketAddr,
) {
    let (mut sender, mut receiver) = ws.split();
    let session_id = match app_state
        .create_wsc_rtp_registration(source_id, client_port)
        .await
    {
        Ok(session_id) => session_id,
        Err(err) => {
            let _ = send_wsc_rtp_message(
                &mut sender,
                WscRtpServerMessage::Error {
                    message: err.to_string(),
                },
            )
            .await;
            return;
        }
    };

    let last_ping = Arc::new(AtomicU64::new(get_current_unix_timestamp()));
    let (ping_timeout_tx, mut ping_timeout_rx) = tokio::sync::oneshot::channel();

    let _last_ping_checker_task = tokio::spawn(last_ping_checker(
        last_ping.clone(),
        ping_timeout_tx,
        session_id.clone(),
    ));

    let udp_holepunch_required = !(client_addr.ip().is_loopback() && client_port.is_some());
    if send_wsc_rtp_message(
        &mut sender,
        WscRtpServerMessage::Init {
            token: session_id.to_string(),
            server_port: app_state.settings().wsc_holepunch_port,
            udp_holepunch_required,
        },
    )
    .await
    .is_err()
    {
        let _ = app_state.delete_wsc_rtp_session(&session_id);
        return;
    }
    log::debug!(
        "wsc-rtp session {} started for stream {} from {}",
        session_id,
        source_id,
        client_addr
    );

    if let Ok(info) = app_state.get_stream(source_id).await {
        let _ = send_wsc_rtp_message(
            &mut sender,
            WscRtpServerMessage::StreamState {
                state: format!("{:?}", info.state),
            },
        )
        .await;
    }

    let mut last_sdp = None;

    loop {
        tokio::select! {
            _ = &mut ping_timeout_rx => {
                log::warn!("wsc-rtp session {} closed due to ping timeout", session_id);
                break;
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {
                if let Ok(sdp) = app_state
                    .try_get_wsc_rtp_sdp_info( &session_id)
                    .await
                {
                    let new_sdp = Some(sdp.clone());
                    if last_sdp != new_sdp {
                        if send_wsc_rtp_message(&mut sender, WscRtpServerMessage::Sdp { sdp }).await.is_ok() {
                            last_sdp = new_sdp;
                        }
                    }
                    continue;
                }
            }
            res = receiver.next() => {
                let msg = match res {
                    Some(Ok(msg)) => msg,
                    Some(Err(err)) => {
                        log::error!("Error receiving WebSocket message: {}", err);
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
                if let Err(err) = handle_wsc_rtp_message(&session_id, Arc::clone(&app_state), &mut sender, source_id, &data, &last_ping).await {
                    log::error!("Error handling wsc-rtp message: {}", err);
                    break;
                }
            }
        }
    }
    log::debug!(
        "wsc-rtp session {} closed for stream {}",
        session_id,
        source_id
    );
    let _ = app_state.delete_wsc_rtp_session(&session_id);
}
