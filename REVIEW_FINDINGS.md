# WSC-RTP and DVR/live review

Reviewed October 28–29, 2026. Source revision: `ad1814a`.

The review combined code inspection, the existing test suite, packet probes against
`http://localhost:8000/media`, and playback on the connected Armor 21 Android device.
The deployed server's exact source revision was not established. During the initial
review, no implementation changes were made. All 25 existing tests passed; they do not cover WSC-RTP
negotiation or DVR/live transitions.

## Recent rewind fix — October 29

Implemented and validated against the long active recording and the native Linux
example client. The isolated test server is available at `http://localhost:8010`;
the production server was not replaced.

The active fMP4 has no finalized random-access index. GStreamer consequently scans
earlier fragments during a seek; the diagnostic had reached only six hours into
the approximately forty-hour recording after ten seconds. Separately, starting
playback before seeking allowed a frame from the recording's beginning to escape.

The fix locates fragments by timestamp using a binary search and exposes a small
virtual MP4 index to the demuxer. Recording bytes are unchanged. Playback prerolls
and seeks while paused, confirms that video is available, and then starts sending
RTP. Subsequent seeks refresh the index, and ongoing playback follows appended
fragments after the index has been loaded. Pipeline watchers belong to the player
so replacing a pipeline also replaces its error watcher.

Validation:

- Native seeks 15 seconds, 30 seconds, 60 seconds, and five minutes behind live
  completed in approximately 25–55 ms. Camera timestamps matched the requested
  positions and continued advancing. Return-to-live restored current footage.
- Starting five seconds behind live, playback progressed another 12.5 seconds,
  crossing the original file boundary and reading newly appended fragments.
- All 33 automated tests pass, including new tests for seek startup, repeated
  seeks, file growth, partial fragments, sparse index positions, invalid metadata,
  virtual reads, and seeking beyond available video. A separate opt-in test against
  an existing H.265 recording also passed. Clippy and formatting checks pass.

Native screenshots and measurement JSON are in `/tmp/dvr-rewind-debug/`.
Other review findings below remain open unless explicitly covered by this fix.

## Initial findings confirmed at runtime

| Priority | Finding | Evidence and impact | Suggested improvement |
| --- | --- | --- | --- |
| High | Recent DVR rewind stalls and shows the wrong recording time | Three seeks approximately 30–60 seconds behind live returned HTTP 200, delivered a brief packet burst, then stalled during each four-second observation window. On Android, a seek 30 seconds back showed an October 27 frame while the UI reported October 29. Returning to live restored playback. The requested position was within the active recording's media timeline; the root cause remains unisolated. | Investigate seeking in the growing fMP4 file. Confirm that playback reaches the selected position before reporting success, and propagate playback errors. |
| High | Initial DVR seek causes an RTP timestamp jump | Seeking five seconds into a finalized recording produced an initial access unit followed by a jump of 410,689 RTP ticks, approximately 4.56 seconds. The pipeline starts playing before its initial seek, allowing early output to consume the stitcher's switch flag. | Gate output while preparing and seeking; adjust timestamps when accepting the selected timeline's first frame. |
| High | DVR → live resumes before a keyframe | All four measured transitions began with dependent H.265 frames. The next keyframe arrived approximately 123, 114, 95, and 480 ms later. Sequence numbers and SSRC remained continuous, but that does not provide the missing decoder references. Android recovered by the screenshot taken about one second after Go Live; the exact visible interruption was not measured. | Resume at a complete random-access frame with the required codec parameters. |
| High | Startup heartbeat expires while pings are buffered | A stopped source with no cached SDP and no UDP hole-punch disconnected after 10.034 seconds despite JSON pings every second. The heartbeat expired around five seconds while startup was still waiting. UDP fallback on a ready source succeeded, so failure depends on startup conditions. | Read WebSocket messages throughout negotiation and use a monotonic heartbeat clock. |
| High | Initialization exposes upstream camera credentials | WSC-RTP `Init.active_source.url` contained the authenticated RTSP URL, while `GET /streams` stripped its userinfo. | Sanitize subscriber-facing source metadata consistently. |
| Medium | First return to live resends identical SDP | The first return to live sent SDP identical to the initial message. Decoder rebuilding by the Android client was not established. | Initialize SDP deduplication with the initial SDP and send updates only when parameters change. |

## Additional issues found by code inspection

- **Old packets can consume the source-switch flag.** Live delivery clones the
  consumer list for an entire frame; removing the consumer does not cancel that
  delivery. DVR's application queue also survives a GStreamer seek, and aborting
  the sender does not await its completion. Use input generations to reject stale
  packets and coordinate switching at complete frame boundaries.
- **Unbounded RTP buffering delays live playback and can exhaust memory.** A mode
  switch does not discard queued DVR media. Bound buffering, discard obsolete
  generations, and recover overflow at a decodable frame.
- **Transport writes can block control handling.** The main WebSocket loop awaits
  writes inside its select branches, preventing heartbeat and control processing
  during a stalled write. Add write deadlines and isolate transport output.
- **SDP, mode changes, and RTP have independent ordering.** New media can precede
  its configuration, and a live notification can precede queued DVR packets.
  Coordinate transition metadata and media; define how UDP clients identify the
  transition.
- **DVR EOS does not advance playback or return to live.** The watcher logs EOS
  and exits; it ignores runtime pipeline errors. Seeking in DVR is restricted to
  the currently opened recording. Define behavior across files, gaps, errors, and
  the available recording edge.
- **Pipeline readiness can be reported too early.** The state waiter accepts a
  matching state change from any child element instead of checking that it came
  from the pipeline itself.
- **Recording timing replaces source PTS/DTS with arrival timing.** Both are set
  to elapsed wall time, losing frame reordering information and incorporating
  network burst timing. Preserve normalized source PTS/DTS and maintain a separate
  wall-clock mapping.
- **Session operations hold DashMap guards across awaits.** Concurrent removal
  can block executor threads. Clone an owned session reference before awaiting.
- **WebSocket EOF continues the receive loop.** `None => continue` can spin until
  timeout; EOF should end the session.
- **`current_timestamp()` mixes units and ignores seek position.** It adds
  nanoseconds to a millisecond Unix timestamp using pipeline running time.
- **Speed control reports success without changing speed.** `set_speed()` only
  logs a warning. Implement it or return an unsupported-operation error.
- **Historical playback uses the current live codec.** Discover the recording's
  codec and parameters instead of assuming they match the current source.

## Relevant code

- `media-server/src/app.rs`: session switching, seek lifecycle, session locking.
- `media-server/src/common/rtp.rs`: consumer delivery and timestamp stitching.
- `media-server/src/sources/dvr.rs`: pipeline preparation, seek, output, and EOS.
- `media-server/src/publishers/wsc_rtp.rs`: negotiation, heartbeat, queues, SDP,
  source metadata, and transport output.
- `media-server/src/domain/dvr/recorder.rs`: recording PTS/DTS assignment.

## Recommended order

1. Isolate and fix active-recording rewind; prevent false playback success.
2. Gate startup output, reject stale input generations, and switch at decodable
   frame boundaries.
3. Fix startup heartbeat handling and sanitize initialization metadata.
4. Bound buffering and coordinate configuration, mode changes, and media.
5. Define DVR boundary/error behavior and correct timing/control APIs.

Regression coverage should include repeated seeks during fragmented frames,
switching with a queued DVR backlog, startup without SDP or UDP, active-file
rewind, EOS, and recordings whose codec differs from the current live source.

## Evidence and limits

Temporary artifacts are in `/tmp/media-server-review/` and may be removed by
system cleanup:

- `websocket-transitions.json`: three recent seeks and returns to live.
- `archive-transitions.json`: finalized-recording seek and timestamp jump.
- `no-sdp-heartbeat.json`: startup heartbeat failure.
- `udp-unavailable.json`: successful fallback on a ready source.
- `active-recording-timing.json`: active fragment timestamps versus wall time.
- `device-archive.png`, `device-return-live.png`, and
  `device-recent-dvr-{1,2}.png`: device playback observations.

Captured JSON artifacts have RTSP credentials redacted. Actual UDP media delivery,
slow-client overload, and every inspection finding were not exercised at runtime.
All temporary sessions and the device port-forwarding mapping were cleaned up.
Existing streams and recordings were left running.
