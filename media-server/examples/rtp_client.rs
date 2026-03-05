//! WSC-RTP Client Example
//!
//! This example demonstrates how to connect to the media server using the
//! WSC-RTP protocol and play the stream using GStreamer.
//!
//! Usage:
//!   cargo run --example rtp_client -- [OPTIONS]
//!
//! Options:
//!   --server <URL>       Server URL (default: http://127.0.0.1:8009)
//!   --source-id <ID>     Stream source ID (default: 1)
//!   --client-port <PORT> UDP port for receiving RTP (default: 5004)
//!   --help               Show this help message
//!
//! Example:
//!   cargo run --example rtp_client -- --server http://localhost:8009 --source-id 1

use gstreamer as gst;
use gstreamer::prelude::*;
use media_server_api_models::{WscRtpClientMessage, WscRtpServerMessage};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket, connect};
use url::Url;

fn print_help() {
    println!(
        r#"WSC-RTP Client Example

USAGE:
    cargo run --example rtp_client -- [OPTIONS]

OPTIONS:
    --server <URL>       Server URL (default: http://127.0.0.1:8009)
    --source-id <ID>     Stream source ID (default: 1)
    --client-port <PORT> UDP port for receiving RTP (default: 5004)
    --help               Show this help message

EXAMPLE:
    cargo run --example rtp_client -- --server http://localhost:8009 --source-id 1
"#
    );
}

fn main() -> anyhow::Result<()> {
    // Initialize GStreamer
    gst::init().map_err(|e| anyhow::anyhow!("Failed to initialize GStreamer: {}", e))?;

    let mut server = "http://127.0.0.1:8009".to_string();
    let mut source_id: String = "1".to_string();
    let mut client_port: u16 = 5004;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            "--server" if i + 1 < args.len() => {
                server = args[i + 1].clone();
                i += 1;
            }
            "--source-id" if i + 1 < args.len() => {
                source_id = args[i + 1].clone();
                i += 1;
            }
            "--client-port" if i + 1 < args.len() => {
                let parsed = args[i + 1]
                    .parse::<u16>()
                    .map_err(|_| anyhow::anyhow!("Invalid --client-port value"))?;
                client_port = parsed;
                i += 1;
            }
            arg => {
                eprintln!("Unknown argument: {}", arg);
                print_help();
                return Err(anyhow::anyhow!("Unknown argument: {}", arg));
            }
        }
        i += 1;
    }

    println!("Connecting to {} for stream {}", server, source_id);

    let server_url = Url::parse(&server)?;
    let ws_url = build_ws_url(&server_url, &source_id)?;
    println!("WebSocket URL: {}", ws_url);

    let (mut socket, _) = connect(&ws_url)?;
    println!("WebSocket connected");

    // Set socket to non-blocking for the main loop
    set_socket_nonblocking(&mut socket, true)?;

    let mut sdp_body: Option<String> = None;
    let mut holepunch_sent = false;

    // Bind the UDP socket early - this socket will receive RTP packets
    let bind_addr = format!("0.0.0.0:{}", client_port);
    let udp_socket = UdpSocket::bind(&bind_addr)?;
    udp_socket.set_nonblocking(true)?;
    println!("UDP socket bound to {}", bind_addr);

    // Phase 1: Wait for Init and SDP
    println!("Waiting for server initialization...");
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(10);

    while sdp_body.is_none() {
        if start.elapsed() > timeout {
            anyhow::bail!("Timeout waiting for SDP");
        }

        // Try to read a message (non-blocking)
        match socket.read() {
            Ok(msg) => {
                let payload = match msg {
                    Message::Text(text) => text.into_bytes(),
                    Message::Binary(data) => data.to_vec(),
                    Message::Ping(payload) => {
                        let _ = socket.send(Message::Pong(payload));
                        continue;
                    }
                    Message::Close(_) => {
                        anyhow::bail!("WebSocket closed before SDP was delivered");
                    }
                    _ => continue,
                };

                let server_msg: WscRtpServerMessage = serde_json::from_slice(&payload)?;
                match server_msg {
                    WscRtpServerMessage::Init {
                        session_id: init_token,
                        holepunch_port: init_port,
                        active_source: _,
                    } => {
                        let udp_holepunch_required = init_port != 0;
                        println!(
                            "Received Init: token={}, port={}, holepunch_required={}",
                            &init_token[..8],
                            init_port,
                            udp_holepunch_required
                        );

                        if udp_holepunch_required && !holepunch_sent {
                            let holepunch_host = server_url
                                .host_str()
                                .ok_or_else(|| anyhow::anyhow!("Server URL missing host"))?;
                            let holepunch_addr: SocketAddr =
                                format!("{}:{}", holepunch_host, init_port).parse()?;
                            let holepunch_payload = format!("t5rtp {} {}", init_token, client_port);
                            println!(
                                "Sending holepunch to {} (payload: {})",
                                holepunch_addr, holepunch_payload
                            );
                            udp_socket.send_to(holepunch_payload.as_bytes(), holepunch_addr)?;
                            holepunch_sent = true;
                        }
                    }
                    WscRtpServerMessage::Sdp { sdp } => {
                        println!("Received SDP ({} bytes)", sdp.len());
                        sdp_body = Some(sdp);
                    }
                    WscRtpServerMessage::SessionMode(mode) => {
                        println!("Session mode: {:?}", mode);
                    }
                    WscRtpServerMessage::Error { message } => {
                        anyhow::bail!("Server error: {}", message);
                    }
                    WscRtpServerMessage::Pong => {
                        // Ignore pong responses
                    }
                    WscRtpServerMessage::FallingBackRtpToWs => {
                        // Ignore fallback notification
                    }
                }
            }
            Err(tungstenite::Error::Io(ref e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No message available, sleep briefly
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                anyhow::bail!("WebSocket error: {}", e);
            }
        }
    }

    let sdp_body = sdp_body.ok_or_else(|| anyhow::anyhow!("Missing SDP body"))?;

    // Parse SDP to extract codec information
    let codec_info = parse_sdp_for_codec(&sdp_body)?;
    println!("Detected codec: {:?}", codec_info);

    // Build GStreamer pipeline based on codec
    let pipeline = build_gstreamer_pipeline(client_port, &codec_info)?;
    println!("GStreamer pipeline created");

    // Start the pipeline
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| anyhow::anyhow!("Failed to set pipeline to Playing: {:?}", e))?;
    println!("GStreamer pipeline started - playing stream");

    // Keep the WebSocket connection alive while playing
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();

    // Handle Ctrl+C
    ctrlc::set_handler(move || {
        running_clone.store(false, Ordering::SeqCst);
    })
    .ok();

    println!("Stream playing. Press Ctrl+C to stop.");

    // Main loop: keep WebSocket alive and handle messages
    let mut last_ping = std::time::Instant::now();
    let ping_interval = Duration::from_secs(3);

    while running.load(Ordering::SeqCst) {
        // Send periodic ping to keep connection alive
        if last_ping.elapsed() > ping_interval {
            let ping_msg = serde_json::to_string(&WscRtpClientMessage::Ping)?;
            if socket.send(Message::Text(ping_msg)).is_err() {
                eprintln!("Failed to send ping, connection may be lost");
                break;
            }
            last_ping = std::time::Instant::now();
        }

        // Process incoming WebSocket messages
        match socket.read() {
            Ok(msg) => {
                match msg {
                    Message::Text(text) => {
                        if let Ok(server_msg) = serde_json::from_str::<WscRtpServerMessage>(&text) {
                            match server_msg {
                                WscRtpServerMessage::Sdp { sdp } => {
                                    // SDP updated (e.g., codec params changed)
                                    println!("SDP updated ({} bytes)", sdp.len());
                                    // Note: In a more sophisticated client, we could reconfigure the pipeline here
                                }
                                WscRtpServerMessage::Error { message } => {
                                    eprintln!("Server error: {}", message);
                                }
                                WscRtpServerMessage::Pong => {
                                    // Keep-alive acknowledged
                                }
                                _ => {}
                            }
                        }
                    }
                    Message::Ping(payload) => {
                        let _ = socket.send(Message::Pong(payload));
                    }
                    Message::Close(_) => {
                        println!("Server closed connection");
                        break;
                    }
                    _ => {}
                }
            }
            Err(tungstenite::Error::Io(ref e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No message available
            }
            Err(e) => {
                eprintln!("WebSocket error: {}", e);
                break;
            }
        }

        thread::sleep(Duration::from_millis(50));
    }

    // Cleanup
    println!("Stopping...");
    pipeline
        .set_state(gst::State::Null)
        .map_err(|e| anyhow::anyhow!("Failed to set pipeline to Null: {:?}", e))?;
    let _ = socket.close(None);

    println!("Done");
    Ok(())
}

fn build_ws_url(base: &Url, source_id: &str) -> anyhow::Result<Url> {
    let mut ws_url = base.clone();
    let scheme = match ws_url.scheme() {
        "https" => "wss",
        "http" => "ws",
        "ws" => "ws",
        "wss" => "wss",
        other => anyhow::bail!("Unsupported server URL scheme: {}", other),
    };
    ws_url
        .set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("Failed to set WebSocket scheme"))?;
    ws_url.set_path(&format!("/streams/{}/wsc-rtp", source_id));
    ws_url.set_query(None);
    Ok(ws_url)
}

fn set_socket_nonblocking(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    nonblocking: bool,
) -> anyhow::Result<()> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => {
            stream.set_nonblocking(nonblocking)?;
        }
        MaybeTlsStream::NativeTls(stream) => {
            stream.get_mut().set_nonblocking(nonblocking)?;
        }
        _ => {
            // For other TLS backends, we can't easily set nonblocking
            // This is fine for most use cases
        }
    }
    Ok(())
}

#[derive(Debug)]
#[allow(dead_code)]
enum CodecInfo {
    H264 { payload_type: u8 },
    H265 { payload_type: u8 },
    Unknown,
}

fn parse_sdp_for_codec(sdp: &str) -> anyhow::Result<CodecInfo> {
    for line in sdp.lines() {
        if line.starts_with("a=rtpmap:") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                let codec_part = parts[1];
                let payload_type = parts[0]
                    .trim_start_matches("a=rtpmap:")
                    .parse::<u8>()
                    .unwrap_or(96);

                if codec_part.starts_with("H264") {
                    return Ok(CodecInfo::H264 { payload_type });
                } else if codec_part.starts_with("H265") || codec_part.starts_with("HEVC") {
                    return Ok(CodecInfo::H265 { payload_type });
                }
            }
        }
    }
    Ok(CodecInfo::Unknown)
}

fn build_gstreamer_pipeline(port: u16, codec_info: &CodecInfo) -> anyhow::Result<gst::Pipeline> {
    let pipeline = gst::Pipeline::new();

    // Create UDP source
    let udp_src = gst::ElementFactory::make("udpsrc")
        .property("port", port as u32)
        .build()
        .map_err(|_| anyhow::anyhow!("Failed to create udpsrc"))?;

    udp_src.set_property("caps", gst::Caps::builder("application/x-rtp").build());

    // Create codec-specific elements
    let (depayloader, parser, decoder): (gst::Element, gst::Element, gst::Element) =
        match codec_info {
            CodecInfo::H264 { .. } => (
                gst::ElementFactory::make("rtph264depay")
                    .build()
                    .map_err(|_| anyhow::anyhow!("Failed to create rtph264depay"))?,
                gst::ElementFactory::make("h264parse")
                    .build()
                    .map_err(|_| anyhow::anyhow!("Failed to create h264parse"))?,
                gst::ElementFactory::make("avdec_h264")
                    .build()
                    .map_err(|_| anyhow::anyhow!("Failed to create avdec_h264"))?,
            ),
            CodecInfo::H265 { .. } => (
                gst::ElementFactory::make("rtph265depay")
                    .build()
                    .map_err(|_| anyhow::anyhow!("Failed to create rtph265depay"))?,
                gst::ElementFactory::make("h265parse")
                    .build()
                    .map_err(|_| anyhow::anyhow!("Failed to create h265parse"))?,
                gst::ElementFactory::make("avdec_h265")
                    .build()
                    .map_err(|_| anyhow::anyhow!("Failed to create avdec_h265"))?,
            ),
            CodecInfo::Unknown => {
                return Err(anyhow::anyhow!(
                    "Unknown codec in SDP, cannot build pipeline"
                ));
            }
        };

    // Add all elements to pipeline
    pipeline
        .add_many([&udp_src, &depayloader, &parser, &decoder])
        .map_err(|e| anyhow::anyhow!("Failed to add elements: {:?}", e))?;

    // Link elements
    udp_src
        .link(&depayloader)
        .map_err(|e| anyhow::anyhow!("Failed to link udpsrc to depayloader: {:?}", e))?;
    depayloader
        .link(&parser)
        .map_err(|e| anyhow::anyhow!("Failed to link depayloader to parser: {:?}", e))?;
    parser
        .link(&decoder)
        .map_err(|e| anyhow::anyhow!("Failed to link parser to decoder: {:?}", e))?;

    // Add video converter and sink
    let videoconvert = gst::ElementFactory::make("videoconvert")
        .build()
        .map_err(|_| anyhow::anyhow!("Failed to create videoconvert"))?;

    let autovideosink = gst::ElementFactory::make("autovideosink")
        .build()
        .map_err(|_| anyhow::anyhow!("Failed to create autovideosink"))?;

    pipeline
        .add_many([&decoder, &videoconvert, &autovideosink])
        .map_err(|e| anyhow::anyhow!("Failed to add elements: {:?}", e))?;

    decoder
        .link(&videoconvert)
        .map_err(|e| anyhow::anyhow!("Failed to link decoder to videoconvert: {:?}", e))?;
    videoconvert
        .link(&autovideosink)
        .map_err(|e| anyhow::anyhow!("Failed to link videoconvert to autovideosink: {:?}", e))?;

    Ok(pipeline)
}
