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
  "token": "550e8400-e29b-41d4-a716-446655440000",
  "holepunch_port": 35000
}
```

**Fields:**
- `token`: UUID session identifier (used for all subsequent operations)
- `holepunch_port`: UDP port allocated for this session's hole-punch listener

### 3. Transport Negotiation

The client should attempt UDP first. If UDP is unavailable, RTP packets will arrive as binary WebSocket frames automatically.

#### UDP Path (preferred — lowest latency)

**Step 1:** Send a UDP packet to `{server_host}:{holepunch_port}`:

```
ws-rtp {TOKEN}
```

**Step 2:** Server responds with dummy UDP packets to confirm connectivity:

```
ws-rtp-dummy {TOKEN}
```

**Step 3:** Client sends an ack over UDP to confirm receipt:

```
ws-rtp-ack {TOKEN}
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
| Hole-punch | Client→Server | UDP | `ws-rtp {token}` |
| Dummy | Server→Client | UDP | `ws-rtp-dummy {token}` |
| Ack | Client→Server | UDP | `ws-rtp-ack {token}` |

## Playback Control (REST API)

All playback control is done via REST API. The session token from `init` is used as `session_id`.

### Base URL
```
http://{server_host}:{port}/streams/{source_id}/wsc-rtp/{session_id}
```

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/mode` | Get current playback mode |
| POST | `/seek` | Seek to timestamp (ms) |
| POST | `/live` | Switch to live mode |
| POST | `/speed` | Set playback speed |

## WebSocket Message Reference

### Client → Server

| Type | Description |
|------|-------------|
| `ping` | Keep-alive heartbeat |

### Server → Client

| Type | Description |
|------|-------------|
| `init` | Session init with token and holepunch port |
| `sdp` | SDP offer with codec parameters |
| `stream_state` | Stream status update |
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
final token = init['token'];
final holepunchPort = init['holepunch_port'];

// 3. Try UDP
final udp = await RawDatagramSocket.bind(InternetAddress.anyIPv4, 0);
udp.send(utf8.encode('ws-rtp $token'), serverAddress, holepunchPort);

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
  if (datagram.data starts with 'ws-rtp-dummy $token') {
    udp.send(utf8.encode('ws-rtp-ack $token'), serverAddress, holepunchPort);
  } else {
    // UDP RTP packet
    decoder.feedRtp(datagram.data);
  }
});
```
