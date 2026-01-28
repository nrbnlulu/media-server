# WSC-RTP Protocol Specification

This document describes the WSC-RTP (WebSocket-Connected RTP) protocol for receiving live and recorded video streams from the media server. This protocol is designed for clients that need low-latency RTP streaming without WebRTC complexity.

## Overview

WSC-RTP uses a hybrid approach:
- **WebSocket**: For signaling, session management, and keep-alive
- **UDP**: For RTP packet delivery (video data)
- **REST API**: For playback control (seek, go-to-live, speed control)

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         Media Server                            │
│                                                                 │
│  ┌─────────────────┐    ┌─────────────────┐                    │
│  │  WebSocket      │    │  UDP Holepunch  │                    │
│  │  Endpoint       │    │  Port (5000)    │                    │
│  │  /streams/{id}/ │    │                 │                    │
│  │  wsc-rtp        │    │                 │                    │
│  └────────┬────────┘    └────────┬────────┘                    │
│           │                      │                              │
│           │  ┌───────────────────┘                              │
│           │  │                                                  │
│           ▼  ▼                                                  │
│  ┌─────────────────────────────────────┐                       │
│  │         Session Manager             │                       │
│  │  - SDP generation                   │                       │
│  │  - RTP packetization               │                       │
│  │  - DVR/Live switching              │                       │
│  └─────────────────────────────────────┘                       │
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
         │ WebSocket            │ UDP (RTP)
         │ (signaling)          │ (video)
         ▼                      ▼
┌─────────────────────────────────────────────────────────────────┐
│                          Client                                  │
│  ┌─────────────────┐    ┌─────────────────┐                    │
│  │  WebSocket      │    │  UDP Socket     │                    │
│  │  Handler        │    │  (RTP receiver) │                    │
│  └─────────────────┘    └─────────────────┘                    │
│           │                      │                              │
│           ▼                      ▼                              │
│  ┌─────────────────────────────────────┐                       │
│  │         RTP Decoder (FFmpeg)        │                       │
│  │  - Parse SDP                        │                       │
│  │  - Decode H.264/H.265               │                       │
│  └─────────────────────────────────────┘                       │
└─────────────────────────────────────────────────────────────────┘
```

## Session Lifecycle

### 1. Establish WebSocket Connection

Connect to the WebSocket endpoint:

```
ws://{server_host}:{port}/streams/{source_id}/wsc-rtp
```

**Parameters:**
- `source_id`: The numeric ID of the stream to subscribe to

### 2. Receive Init Message

Upon connection, the server sends an `init` message:

```json
{
  "type": "init",
  "token": "550e8400-e29b-41d4-a716-446655440000",
  "server_port": 5000,
  "udp_holepunch_required": true
}
```

**Fields:**
- `token`: UUID session identifier (used for all subsequent operations)
- `server_port`: UDP port for holepunch packet
- `udp_holepunch_required`: Whether NAT traversal is needed
  - `true`: Client is on a different network, must send holepunch packet
  - `false`: Client is local (loopback), can skip holepunch

### 3. UDP Holepunch (NAT Traversal)

If `udp_holepunch_required` is `true`, send a UDP packet to establish the return path:

**Packet Format:**
```
t5rtp {TOKEN} {CLIENT_PORT}
```

**Example:**
```
t5rtp 550e8400-e29b-41d4-a716-446655440000 5004
```

**Parameters:**
- `t5rtp`: Fixed protocol header
- `TOKEN`: The session token from the `init` message
- `CLIENT_PORT`: The local UDP port where the client will receive RTP packets

**Send to:**
- Address: `{server_host}:{server_port}` (server_port from init message)

**Behavior:**
- For local/private networks: Server uses the provided `CLIENT_PORT`
- For public networks: Server uses the NAT-translated source address

### 4. Receive SDP

After holepunch, the server sends the SDP offer:

```json
{
  "type": "sdp",
  "sdp": "v=0\r\no=- 123456 0 IN IP4 0.0.0.0\r\n..."
}
```

**SDP Contents:**
- Session description with connection info
- Media description for video
- Codec information (H.264 or H.265)
- FMTP parameters (SPS/PPS for H.264, VPS/SPS/PPS for H.265)

**Example SDP:**
```
v=0
o=- 123456 0 IN IP4 0.0.0.0
s=media-server
c=IN IP4 0.0.0.0
t=0 0
m=video 5004 RTP/AVP 96
a=rtpmap:96 H264/90000
a=fmtp:96 packetization-mode=1;sprop-parameter-sets=Z0IAKeKQFAe2AtwEBAaQeJEV,aM48gA==
a=sendonly
```

**Important:** The SDP may be sent multiple times if codec parameters change. Always use the latest SDP.

### 5. Start Receiving RTP

Once you have the SDP, configure your decoder and start receiving RTP packets on your UDP socket.

**RTP Packet Structure:**
```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|V=2|P|X|  CC   |M|     PT      |       Sequence Number         |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                           Timestamp                           |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                             SSRC                              |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                         RTP Payload                           |
|                             ...                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

**Fields:**
- `V`: Version (always 2)
- `P`: Padding (0)
- `X`: Extension (0)
- `CC`: CSRC count (0)
- `M`: Marker bit (1 = end of frame)
- `PT`: Payload type (96 for dynamic)
- `Sequence Number`: 16-bit, monotonically increasing
- `Timestamp`: 90kHz clock (video standard)
- `SSRC`: Synchronization source identifier

### 6. Keep-Alive (Ping/Pong)

Send periodic ping messages to keep the session alive:

**Client sends:**
```json
{"type": "ping"}
```

**Server responds:**
```json
{"type": "pong"}
```

**Timeout:** The server will close the connection after 5 seconds without a ping.

**Recommended interval:** Send ping every 2-3 seconds.

### 7. Stream State Notifications

The server sends stream state updates:

```json
{
  "type": "stream_state",
  "state": "Active"
}
```

**Possible states:**
- `Active`: Stream is receiving video
- `Inactive`: Stream is not receiving video
- `Connecting`: Stream is connecting to source
- `Error`: Stream encountered an error

## Playback Control (REST API)

All playback control is done via REST API endpoints. The session token from the `init` message is used as the `session_id`.

### Base URL
```
http://{server_host}:{port}/streams/{source_id}/wsc-rtp/{session_id}
```

### Get Current Mode

**Request:**
```http
GET /streams/{source_id}/wsc-rtp/{session_id}/mode
```

**Response:**
```json
{
  "is_live": true,
  "current_time_ms": null,
  "speed": 1.0
}
```

Or for DVR mode:
```json
{
  "is_live": false,
  "current_time_ms": 1706369234567,
  "speed": 1.0
}
```

### Seek to Timestamp (DVR)

Seek to a specific timestamp in recorded video.

**Request:**
```http
POST /streams/{source_id}/wsc-rtp/{session_id}/seek
Content-Type: application/json

{
  "timestamp": 1706369234567
}
```

**Parameters:**
- `timestamp`: Unix timestamp in milliseconds

**Response:**
```json
{
  "is_live": false,
  "current_time_ms": 1706369234567,
  "speed": 1.0
}
```

**Notes:**
- Automatically switches from live to DVR mode if needed
- Seeks to the nearest keyframe
- RTP sequence numbers remain continuous (stitching)

### Switch to Live

Return to live streaming from DVR mode.

**Request:**
```http
POST /streams/{source_id}/wsc-rtp/{session_id}/live
```

**Response:**
```json
{
  "is_live": true,
  "current_time_ms": null,
  "speed": 1.0
}
```

**Notes:**
- Stops DVR playback
- Resumes live RTP stream
- RTP sequence numbers remain continuous

### Set Playback Speed (DVR only)

**Request:**
```http
POST /streams/{source_id}/wsc-rtp/{session_id}/speed
Content-Type: application/json

{
  "speed": 2.0
}
```

**Parameters:**
- `speed`: Playback speed multiplier (e.g., 0.5, 1.0, 2.0, 4.0)

**Response:**
```json
{
  "is_live": false,
  "current_time_ms": 1706369234567,
  "speed": 2.0
}
```

**Notes:**
- Only works in DVR mode
- Returns error if currently in live mode

## WebSocket Message Reference

### Client → Server Messages

| Message Type | Description |
|--------------|-------------|
| `ping` | Keep-alive heartbeat |

**Example:**
```json
{"type": "ping"}
```

### Server → Client Messages

| Message Type | Description |
|--------------|-------------|
| `init` | Session initialization with token and holepunch info |
| `sdp` | SDP offer with codec parameters |
| `stream_state` | Stream status update |
| `pong` | Response to ping |
| `error` | Error message |

**Init Message:**
```json
{
  "type": "init",
  "token": "550e8400-e29b-41d4-a716-446655440000",
  "server_port": 5000,
  "udp_holepunch_required": true
}
```

**SDP Message:**
```json
{
  "type": "sdp",
  "sdp": "v=0\r\n..."
}
```

**Stream State Message:**
```json
{
  "type": "stream_state",
  "state": "Active"
}
```

**Pong Message:**
```json
{"type": "pong"}
```

**Error Message:**
```json
{
  "type": "error",
  "message": "Session not found"
}
```

## RTP Payload Formats

### H.264 (RFC 6184)

**Payload Type:** 96 (dynamic)

**NAL Unit Types in Payload:**
- Single NAL unit: NAL header + data
- FU-A fragmentation for large NALs

**FU-A Header (2 bytes):**
```
+---------------+
|0|1|2|3|4|5|6|7|
+-+-+-+-+-+-+-+-+
|F|NRI|  Type   | FU indicator (Type=28 for FU-A)
+---------------+
|S|E|R|  Type   | FU header
+---------------+
```

### H.265 (RFC 7798)

**Payload Type:** 96 (dynamic)

**NAL Unit Types in Payload:**
- Single NAL unit: 2-byte NAL header + data
- FU fragmentation for large NALs

**FU Header (3 bytes):**
```
+---------------+---------------+
|0|1|2|3|4|5|6|7|0|1|2|3|4|5|6|7|
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|F|  Type=49 |  LayerId  | TID | FU indicator
+---------------+---------------+
|S|E|    FuType     |           | FU header
+---------------+---------------+
```

## Implementation Notes

### Sequence Continuity

The server uses RTP stitching to maintain sequence number continuity when:
- Switching between live and DVR modes
- Seeking within DVR
- Recovering from stream interruptions

Clients should handle sequence gaps gracefully but can rely on mostly continuous sequences.

### Timestamp Handling

- RTP timestamps use 90kHz clock (standard for video)
- Timestamps are adjusted during mode switches for seamless playback
- Client should use timestamps for frame ordering, not wall-clock time

### Error Handling

1. **WebSocket disconnection**: Re-establish connection and get new session
2. **UDP packet loss**: Decoder handles this (P-frame corruption until next I-frame)
3. **SDP changes**: Re-configure decoder with new parameters
4. **REST API errors**: Check response body for error message

### Recommended Client Implementation

1. **Initialization:**
   - Connect WebSocket
   - Parse `init` message
   - Create UDP socket on random port
   - Send holepunch packet if required
   - Wait for SDP

2. **Playback:**
   - Parse SDP for codec parameters
   - Configure FFmpeg/decoder
   - Start UDP receive loop
   - Feed RTP packets to decoder

3. **Keep-alive:**
   - Send ping every 2-3 seconds
   - Reconnect if no pong received

4. **Controls:**
   - Use REST API for seek/live/speed
   - Handle SDP updates after mode changes

## Example Client Flow (Pseudocode)

```dart
// 1. Connect WebSocket
ws = WebSocket.connect("ws://server:8080/streams/123/wsc-rtp");

// 2. Handle init message
initMsg = await ws.receive();
token = initMsg.token;
serverPort = initMsg.server_port;

// 3. Create UDP socket and holepunch
udpSocket = UdpSocket.bind("0.0.0.0", 0);
clientPort = udpSocket.localPort;

if (initMsg.udp_holepunch_required) {
  holepunchPacket = "t5rtp $token $clientPort";
  udpSocket.send(holepunchPacket, serverAddress, serverPort);
}

// 4. Wait for SDP
sdpMsg = await ws.receive();
decoder.configure(sdpMsg.sdp);

// 5. Start receive loop
while (true) {
  select {
    case rtpPacket = udpSocket.receive():
      decoder.feedRtpPacket(rtpPacket);
    
    case wsMsg = ws.receive():
      if (wsMsg.type == "sdp") {
        decoder.reconfigure(wsMsg.sdp);
      }
    
    case <-pingTimer:
      ws.send({"type": "ping"});
  }
}

// 6. Seek example
http.post("/streams/123/wsc-rtp/$token/seek", 
  body: {"timestamp": 1706369234567});

// 7. Go to live
http.post("/streams/123/wsc-rtp/$token/live");
```

## FFmpeg Integration Notes

For Flutter clients using FFmpeg (rffmpeg):

1. **SDP File:** Write the SDP to a temporary file or use a data URI
2. **FFmpeg Input:** Use `-protocol_whitelist file,udp,rtp -i {sdp_path}`
3. **RTP Demuxer:** FFmpeg's RTP demuxer handles depacketization
4. **Codec Detection:** FFmpeg reads codec from SDP `a=rtpmap` and `a=fmtp` lines

**Example FFmpeg command equivalent:**
```bash
ffmpeg -protocol_whitelist file,udp,rtp \
       -i session.sdp \
       -c:v copy \
       -f rawvideo -
```

## Appendix: Full REST API Reference

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/streams/{source_id}/wsc-rtp/{session_id}/mode` | Get current playback mode |
| POST | `/streams/{source_id}/wsc-rtp/{session_id}/seek` | Seek to timestamp |
| POST | `/streams/{source_id}/wsc-rtp/{session_id}/live` | Switch to live mode |
| POST | `/streams/{source_id}/wsc-rtp/{session_id}/speed` | Set playback speed |
