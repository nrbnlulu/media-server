# Media Server - Agent Documentation

## Overview

This is a **high-performance realtime media streaming server** written in Rust, optimized for live streaming and DVR (Digital Video Recording) with live-rewind capabilities. It acts as a proxy/gateway that ingests RTSP camera streams and distributes them to multiple consumers via modern protocols (WebRTC and a custom WSC-RTP protocol).

## Goals & Purpose

- **Ingest** video from RTSP sources (IP cameras, RTSP servers)
- **Stream** to multiple clients simultaneously via WebRTC
- **Provide** alternative low-latency RTP streaming via WSC-RTP (WebSocket-Connected RTP)
- **Record** streams to disk for DVR/playback functionality
- **Enable** live-rewind capability (seek in recorded video while maintaining live mode)
- **Handle** camera disconnections and automatic reconnection with fallback mechanisms

## Architecture

### High-Level Data Flow

```
RTSP Source (Camera) 
    |
    v
RtspClient (Source)
    |-- RtpPacketizer (transforms NAL units to RTP packets)
    |       |
    |       |-- WebRtcSession (Consumer - WebRTC)
    |       |-- WscRtpPublisher (Consumer - WSC-RTP)
    |
    |-- DvrRecorder (records to disk)
            |
            v
        MP4 Files
            |
            v
        DvrPlayer (for playback/seeking)
```

### Core Components

#### Sources (`media-server/src/sources/`)

| Component | File | Description |
|-----------|------|-------------|
| **RtspClient** | `rtsp/mod.rs` | Ingests RTSP streams using FFmpeg, implements `VideoSource` trait, auto-reconnects on failure |
| **DvrPlayer** | `dvr.rs` | Replays recorded video files using GStreamer pipelines, supports seeking |

#### Publishers (`media-server/src/publishers/`)

| Component | File | Description |
|-----------|------|-------------|
| **WebRtcSession** | `webrtc.rs` | WebRTC peer connection management using `webrtc-rs`, ICE/STUN handling |
| **WscRtpPublisher** | `wsc_rtp.rs` | Custom low-latency RTP over UDP with WebSocket signaling, UDP hole-punching |

#### Common Infrastructure (`media-server/src/common/`)

| Component | File | Description |
|-----------|------|-------------|
| **RtpPacketizer** | `rtp.rs` | Converts NAL units to RTP packets (RFC 3550, RFC 6184/7798), manages multiple consumers |
| **StitchingConsumer** | `rtp.rs` | Bridges live/DVR modes, maintains RTP sequence/timestamp continuity |
| **NAL Utils** | `nal_utils.rs` | H.264/H.265 NAL unit parsing and codec detection |

#### Domain Models (`media-server/src/domain/`)

| Component | File | Description |
|-----------|------|-------------|
| **DvrRecorder** | `dvr/recorder.rs` | Records video to MP4 using GStreamer |
| **Filesystem** | `dvr/filesystem.rs` | Manages DVR directory structure and recording metadata |

#### Application State (`media-server/src/app.rs`)

| Component | Description |
|-----------|-------------|
| **GlobalState** | Central manager holding all active sources and sessions |
| **ClientSession** | Per-client session state, handles mode switching (Live/DVR) |

### Key Traits

```rust
pub trait VideoSource: Send + Sync { ... }      // RTSP, DVR sources
pub trait FfmpegConsumer: Send + Sync { ... }   // RTP packetizer, recorder
pub trait RtpConsumer: Send + Sync { ... }      // WebRTC, WSC-RTP
pub trait RtpVideoPublisher: RtpConsumer { ... } // Combined trait
```

## Features

### Streaming Protocols

| Protocol | Type | Use Case | Details |
|----------|------|----------|---------|
| **RTSP** | Source | Camera input | Standard IP camera protocol, auto-reconnect on failure |
| **WebRTC** | Consumer | Web/mobile clients | Low-latency, NAT-friendly, ICE candidates via STUN |
| **WSC-RTP** | Consumer | Custom clients | Ultra-low latency, UDP hole-punching, minimal overhead |
| **REST API** | Control | Playback control | Seek, speed, mode switching, stream management |

### Codec Support

- **H.264** (AVC/MPEG-4 Part 10) - Full support with SPS/PPS extraction
- **H.265** (HEVC/MPEG-H Part 2) - Full support with VPS/SPS/PPS extraction

### DVR/Recording Features

| Feature | Status | Details |
|---------|--------|---------|
| Live Recording | Implemented | Automatic recording to MP4 when enabled |
| Live-Rewind | Implemented | Seek into recorded video, return to live |
| Playback Speed | Partial | Framework present |
| H.265 Transcoding | Planned | Automatic transcoding to VP9 |
| VOD Archival | Planned | Long-term storage (days/weeks/months) |

### API Endpoints

**Stream Management:**
- `POST /streams` - Create stream from RTSP URL
- `GET /streams` - List all streams
- `GET /streams/{source_id}` - Get stream details
- `DELETE /streams/{source_id}` - Stop and delete stream

**WebRTC Sessions:**
- `POST /streams/{source_id}/webrtc` - Create live WebRTC session
- `POST /streams/{source_id}/webrtc/dvr` - Create DVR WebRTC session
- `GET /streams/{source_id}/webrtc` - List active sessions
- `DELETE /streams/{source_id}/webrtc/{session_id}` - Close session

**Playback Control:**
- `POST /streams/{source_id}/webrtc/{session_id}/seek` - Seek to timestamp
- `POST /streams/{source_id}/webrtc/{session_id}/live` - Return to live
- `POST /streams/{source_id}/webrtc/{session_id}/speed` - Set playback speed
- `GET /streams/{source_id}/webrtc/{session_id}/mode` - Get current mode

**WSC-RTP:**
- `GET /streams/{source_id}/wsc-rtp` - WebSocket upgrade endpoint

**Utilities:**
- `GET /health` - Health check
- `GET /swagger-ui` - API documentation
- `GET /debug/webrtc-client` - WebRTC test client

## Tech Stack

### Language & Runtime

- **Language:** Rust 2021 edition
- **Async Runtime:** Tokio 1.35 (full features)
- **Project Structure:** Cargo workspace with 2 crates

### Core Dependencies

| Category | Dependencies |
|----------|-------------|
| **Web Framework** | axum 0.7, tower 0.4, tower-http 0.5 |
| **Media Processing** | rffmpeg (FFmpeg bindings), gstreamer 0.22 |
| **WebRTC** | webrtc 0.9 |
| **Serialization** | serde 1.0, serde_json 1.0 |
| **Concurrency** | dashmap 5.5, parking_lot 0.12.5, futures 0.3 |
| **API Docs** | utoipa 4.2, utoipa-swagger-ui 6.0 |
| **Utilities** | anyhow, thiserror, log, env_logger, uuid, chrono |

### External Protocols

| Protocol | Purpose |
|----------|---------|
| RTSP | Camera stream ingestion |
| RTP/AVP | Packet delivery (RFC 3550) |
| H.264 Payload | Video encoding (RFC 6184) |
| H.265 Payload | Video encoding (RFC 7798) |
| SDP | Session description (RFC 4566) |
| WebRTC | Client streaming with ICE/STUN |
| MP4/ISO Base Media | Recording format |

## Project Structure

```
/media-server/
├── Cargo.toml                 # Workspace root
├── README.md                  # Project overview
├── AGENTS.md                  # This file
├── media-server/
│   ├── Cargo.toml             # Main crate
│   ├── src/
│   │   ├── main.rs            # Entry point
│   │   ├── app.rs             # GlobalState, ClientSession
│   │   ├── utils.rs           # Utilities
│   │   ├── api/
│   │   │   ├── mod.rs         # Router creation
│   │   │   ├── handlers.rs    # HTTP endpoint handlers
│   │   │   └── models.rs      # Request/response DTOs
│   │   ├── sources/
│   │   │   ├── mod.rs
│   │   │   ├── rtsp/mod.rs    # RTSP source implementation
│   │   │   └── dvr.rs         # DVR player implementation
│   │   ├── publishers/
│   │   │   ├── mod.rs
│   │   │   ├── webrtc.rs      # WebRTC session management
│   │   │   └── wsc_rtp.rs     # WSC-RTP UDP publisher
│   │   ├── common/
│   │   │   ├── mod.rs
│   │   │   ├── traits.rs      # Core traits
│   │   │   ├── rtp.rs         # RTP packet handling
│   │   │   └── nal_utils.rs   # NAL unit parsing
│   │   └── domain/
│   │       ├── mod.rs         # StreamConfig, StreamState
│   │       └── dvr/
│   │           ├── mod.rs
│   │           ├── recorder.rs # DVR recording
│   │           └── filesystem.rs # DVR file management
│   ├── WSC-RTP-PROTOCOL.md    # Protocol specification
│   └── TODO.md                # Development roadmap
└── crates/
    └── media-server-api-models/
        ├── Cargo.toml
        └── src/lib.rs         # Shared API models
```

## Configuration

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `BIND_ADDRESS` | `0.0.0.0:8009` | HTTP server bind address |
| `WSC_HOLEPUNCH_PORT` | `5000` | UDP hole-punch port |

### Stream Configuration

```rust
pub struct StreamConfig {
    pub source_id: u64,
    pub rtsp_url: url::Url,
    pub username: Option<String>,
    pub password: Option<String>,
    pub should_record: bool,
    pub restart_interval_secs: Option<u64>,
}
```

### CLI Options

- `--port {PORT}` - HTTP server port (default: 8009)

## Architectural Patterns

### Producer-Consumer Pattern
- RtspClient/DvrPlayer = Producers (implement `FfmpegConsumer` trait)
- WebRtcSession/WscRtpPublisher = Consumers (implement `RtpConsumer` trait)
- RtpPacketizer = Mediator (manages multiple consumers)

### Concurrent State Management
- `DashMap` for lock-free concurrent hashmaps
- `TokioMutex` for async-safe exclusive access
- `parking_lot::Mutex` for fast sync locks
- `Arc` for shared ownership across async tasks

### Mode Switching with Sequence Continuity
- `StitchingConsumer` maintains RTP sequence numbers across Live/DVR transitions
- Timestamp offset calculation prevents discontinuities
- Clients see seamless RTP stream despite underlying source changes

### Error Recovery
- RtspClient automatically reconnects on stream failure
- Fallback pipeline generation when source is offline
- Graceful consumer cleanup on disconnect

## Build & Run

### Prerequisites

- Rust (2021 edition)
- FFmpeg libraries
- GStreamer libraries

### Build

```bash
cargo build --release
```

Release profile optimizations:
- `opt-level = 3` - Maximum optimization
- `lto = true` - Link-time optimization
- `codegen-units = 1` - Single codegen unit for better optimization

### Run

```bash
cargo run --release -- --port 8009
```

## Development Notes

### Key Files to Understand

1. `media-server/src/app.rs` - Central state management, start here
2. `media-server/src/sources/rtsp/mod.rs` - RTSP ingestion logic
3. `media-server/src/common/rtp.rs` - RTP packetization and consumer management
4. `media-server/src/publishers/webrtc.rs` - WebRTC session handling
5. `media-server/src/api/handlers.rs` - API endpoint implementations

### Adding a New Feature

1. Define traits in `common/traits.rs` if needed
2. Implement source in `sources/` or publisher in `publishers/`
3. Register with `GlobalState` in `app.rs`
4. Add API endpoints in `api/handlers.rs` and `api/mod.rs`
5. Update models in `api/models.rs` or `crates/media-server-api-models/`

### Testing

Use the debug WebRTC client at `/debug/webrtc-client` for testing streams.

## Status

**Implemented:**
- RTSP stream ingestion with auto-reconnect
- WebRTC streaming to clients
- WSC-RTP custom low-latency protocol
- Live recording to MP4
- DVR playback with seeking
- H.264 and H.265 support
- Multi-consumer support
- API with Swagger documentation
- Graceful shutdown

**In Progress:**
- Playback speed control

**Planned:**
- H.265 automatic transcoding to VP9
- VOD (long-term archival)
- GOP caching for RTSP optimization
