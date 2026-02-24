# WSC-RTP Protocol Specification

This document describes the WSC-RTP (WebSocket-Connected RTP) protocol for receiving live and recorded video streams from the media server. This protocol is designed for clients that need low-latency RTP streaming without WebRTC complexity.

## Overview

WSC-RTP uses a hybrid approach:
- **WebSocket**: For signaling, session management, keep-alive, and RTP fallback transport
- **UDP**: For RTP packet delivery (preferred — lowest latency)
- **REST API**: For playback control (seek, go-to-live, speed control)

When UDP is not available (e.g. blocked by firewall or NAT), RTP packets are automatically delivered as binary WebSocket frames instead.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         Media Server                            │
│                                                                 │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │  WebSocket Endpoint: /streams/{id}/wsc-rtp              │   │
│  │  - Signaling (JSON text frames)                         │   │
│  │  - RTP fallback (binary frames, when UDP unavailable)   │   │
│  └─────────────────────────────────────────────────────────┘   │
│                                                                 │
│  ┌──────────────────────────────────┐                          │
│  │  UDP Holepunch Port (30000-40000) │                          │
│  └──────────────────────────────────┘                          │
│                                                                 │
│  ┌─────────────────┐                                           │
│  │  REST API       │                                           │
│  │  - /seek        │                                           │
│  │  - /live        │                                           │
│  │  - /speed       │                                           │
│  │  - /mode        │                                           │
│  └─────────────────┘                                           │
└─────────────────────────────────────────────────────────────────┘
         │                      │
         │ WebSocket            │ UDP (preferred)
         │ (signaling +         │ (video)
         │  RTP fallback)       │
         ▼                      ▼
┌─────────────────────────────────────────────────────────────────┐
│                          Client                                  │
└─────────────────────────────────────────────────────────────────┘
```

## Query Parameters

The WebSocket endpoint supports optional query parameters:

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `force_websocket_transport` | boolean | `false` | Skip UDP negotiation and use WebSocket for RTP delivery from the start. Useful for clients behind restrictive NAT. |

**Example:**
```
ws://server/streams/{id}/wsc-rtp?force_websocket_transport=true
```

## Session Lifecycle

### 1. Establish WebSocket Connection

Connect to the WebSocket endpoint:

```
ws://{server_host}:{port}/streams/{source_id}/wsc-rtp
```

### 2. Receive Init Message

Upon connection, the server sends an `init` message:

```json
{
  "type": "init",
  "session_id": "550e8400-e29b-41d4-a716-446655440000",
  "holepunch_port": 35000,
  "active_source": { "label": "primary", "url": "rtsp://...", "priority": 0 }
}
```

**Fields:**
- `session_id`: UUID session identifier (used for all subsequent REST API calls and UDP hole-punch)
- `holepunch_port`: UDP port allocated for this session's hole-punch listener
- `active_source`: The currently active RTSP input source (`null` if stream not yet connected)

### 3. Transport Negotiation

The client should attempt UDP first. If UDP is unavailable, RTP packets will arrive as binary WebSocket frames automatically.

#### UDP Path (preferred — lowest latency)

**Step 1:** Send a UDP packet to `{server_host}:{holepunch_port}`:

```
ws-rtp {session_id}
```

**Step 2:** Server responds with dummy UDP packets to confirm connectivity:

```
ws-rtp-dummy {session_id}
```

**Step 3:** Client sends an ack over UDP to confirm receipt:

```
ws-rtp-ack {session_id}
```

**Step 4:** Server confirms UDP and streams RTP packets over UDP.

#### WebSocket Fallback (automatic)

If the server does not receive a UDP ack within 5 seconds of the hole-punch, it automatically switches to sending RTP packets as **binary WebSocket frames**. The client will start receiving binary frames on the existing WebSocket connection — no extra action needed.

### 4. Receive SDP

After transport is established, the server sends the SDP over WebSocket:

```json
{
  "type": "sdp",
  "sdp": "v=0\r\no=- 123456 0 IN IP4 0.0.0.0\r\n..."
}
```

The SDP may be sent multiple times if codec parameters change. Always use the latest SDP.

### 5. Receiving RTP

#### UDP

Raw RTP bytes — no additional framing.

#### WebSocket Fallback

RTP packets arrive as binary WebSocket frames. Each frame is exactly one RTP packet.

### 6. Keep-Alive (Ping/Pong)

```json
{"type": "ping"}   // Client → Server, every 2-3 seconds
{"type": "pong"}   // Server → Client
```

Server closes connection after 5 seconds without a ping.

### 7. Stream State Notifications

```json
{
  "type": "stream_state",
  "state": "Active"
}
```

States: `Active`, `Inactive`, `Connecting`, `Error`

## Holepunch Message Reference

| Message | Sender | Transport | Format |
|---------|--------|-----------|--------|
| Hole-punch | Client→Server | UDP | `ws-rtp {session_id}` |
| Dummy | Server→Client | UDP | `ws-rtp-dummy {session_id}` |
| Ack | Client→Server | UDP | `ws-rtp-ack {session_id}` |

## Playback Control (REST API)

All playback control is done via REST API. The `session_id` from the `init` message is used as the path parameter.

### Base URL
```
http://{server_host}:{port}/client-session-control/{session_id}
```

| Method | Endpoint | Body | Description |
|--------|----------|------|-------------|
| GET | `/mode` | — | Get current playback mode |
| POST | `/seek` | `{"timestamp": <ms>}` | Seek to timestamp (ms since epoch) |
| POST | `/live` | — | Switch to live mode |
| POST | `/speed` | `{"speed": <float>}` | Set playback speed (DVR only) |
| DELETE | `` | — | Close and delete the session |

## WebSocket Message Reference

### Client → Server

| Type | Description |
|------|-------------|
| `ping` | Keep-alive heartbeat |

### Server → Client

| Type | Description |
|------|-------------|
| `init` | Session init with `session_id`, `holepunch_port`, and `active_source` |
| `sdp` | SDP offer with codec parameters |
| `session_mode` | Current playback mode (`live` or `dvr` with timestamp) |
| `falling_back_rtp_to_ws` | UDP transport failed; RTP will now arrive as binary WebSocket frames |
| `pong` | Response to ping |
| `error` | Error message |

## RTP Payload Formats

### H.264 (RFC 6184) — Payload Type 96
- Single NAL unit or FU-A fragmentation for large NALs

### H.265 (RFC 7798) — Payload Type 96
- Single NAL unit or FU fragmentation for large NALs

## Implementation Notes

- RTP timestamps use 90kHz clock
- Sequence numbers are continuous across Live/DVR transitions (server-side stitching)
- SDP changes should trigger decoder reconfiguration

## Example Client Flow

```dart
// 1. Connect WebSocket
final ws = WebSocket.connect("ws://server:8080/streams/123/wsc-rtp");

// 2. Receive init
final init = await receiveJson(ws);
final sessionId = init['session_id'];
final holepunchPort = init['holepunch_port'];

// 3. Try UDP
final udp = await RawDatagramSocket.bind(InternetAddress.anyIPv4, 0);
udp.send(utf8.encode('ws-rtp $sessionId'), serverAddress, holepunchPort);

// 4. Wait for SDP (arrives after transport is established)
//    Meanwhile: if UDP dummy packets arrive, send ack
//    If no UDP activity, RTP will come over WebSocket binary frames

// 5. Receive loop
ws.listen((frame) {
  if (frame is String) {
    final msg = jsonDecode(frame);
    if (msg['type'] == 'sdp') decoder.configure(msg['sdp']);
  } else if (frame is List<int>) {
    // WebSocket fallback RTP
    decoder.feedRtp(frame);
  }
});

udp.listen((event) {
  final datagram = udp.receive();
  if (datagram.data starts with 'ws-rtp-dummy $sessionId') {
    udp.send(utf8.encode('ws-rtp-ack $sessionId'), serverAddress, holepunchPort);
  } else {
    // UDP RTP packet
    decoder.feedRtp(datagram.data);
  }
});
```
