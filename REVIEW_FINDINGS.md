# WSC-RTP and DVR/live review

Reviewed October 28–29, 2026. Source revision: `ad1814a`.

The review combined code inspection, the existing test suite, packet probes against
`http://localhost:8000/media`, and playback on the connected Armor 21 Android device.
The deployed server's exact source revision was not established. During the initial
review, no implementation changes were made. All 25 existing tests passed; they do not cover WSC-RTP
negotiation or DVR/live transitions.

## Segmented recording migration — October 29

The previous rewind fix is committed as `f49cbe8`. The migration is implemented
in the working tree and left uncommitted for review. New recordings use segmented
MP4; existing single-file recordings continue through the legacy player and its
virtual-index adapter. Existing recording bytes have not been converted.

New recording layout:

```text
dvr/<source hash>/segments-<session epoch>-<uuid>/
  0000000000.mp4
  0000000000.json
  0000000001.mp4
  0000000001.json
  ...
```

`splitmuxsink` rotates at approximately ten seconds, aligned to camera keyframes,
using asynchronous finalization. A JSON sidecar is atomically published only after
the corresponding MP4 is closed. It records the codec, session epoch and precise
segment timestamp range. Disk playback uses standard `qtdemux` and codec parsers;
new recordings do not use the custom MP4 index or parse MP4 boxes in application
code. A short encoded-video buffer covers the active segment and overlaps completed
files. It is bounded by thirty seconds and 64 MiB per source, evicting complete
GOPs so a retained buffer starts at a keyframe.

The custom application logic is the segment catalog, recent buffer and playback
coordinator. Completed segments are read with bounded memory and the next batch
is prepared while the current batch plays. One RTP packetizer and one absolute
media clock span the disk files and memory. Both storage paths use microsecond
precision, allowing an exact overlap-frame match rather than a timestamp tolerance.
Explicit seeks prepare their replacement first, then stop and await the previous
sender before switching. Seeks can cross segments and select legacy recordings.

Native validation also found two control-time defects: subsequent seeks did not
send a new DVR mode event, and the mode API returned a file-relative position where
the Linux client expects a Unix timestamp. Successful seeks now notify the client,
and the mode response reports the current frame's absolute timestamp.

Validation:

- `cargo test --workspace --locked`: 37 passed, one existing optional recording
  probe ignored. `cargo clippy --workspace --all-targets --locked -- -D warnings`,
  formatting and whitespace checks passed.
- H.264 and H.265 recorder round trips preserve every generated frame timestamp
  through multiple finalized files. The test includes a nonzero, sub-millisecond
  starting offset.
- Playback verifies every expected frame timestamp, RTP sequence continuity and
  unchanged SSRC across file boundaries and disk-to-memory handoff. Further tests
  cover recent frames arriving during playback, failed seeks, restart without the
  memory buffer and GOP-aligned byte-budget eviction.
- The native Linux example played the camera through the isolated server. Five-,
  fifteen- and forty-five-second rewinds were accepted in approximately 2–9 ms.
  The forty-five-second rewind continued for another thirty-eight seconds across
  disk files into recent footage. Repeated seek labels were checked again after
  the control-time fix: the UI and camera clock agree. Return-to-live worked.

The isolated server remains at `http://localhost:8010`, with test recordings under
`/tmp/segmented-dvr-review/recordings`. The native client is back in live mode.
Production was not replaced and its DVR directory was not modified.

Limits and follow-up work:

- The unfinished MP4 and in-memory tail are not recovered after a crash. Completed,
  cataloged files remain playable; the unfinished file is retained but excluded
  from playback. The lost tail can exceed ten seconds if keyframes are far apart.
- Independent segments require suitable closed GOPs from the camera. Rotation
  follows existing keyframes and does not re-encode or change camera settings.
- Catalog discovery currently scans segment sidecars. A large-history catalog
  should be indexed before scaling to very long retention or many simultaneous
  historical viewers. Automatic retention deletion is not implemented.
- Playback rejects a segment exceeding its 64 MiB read budget. Very high bitrate
  streams or unusually long GOPs need an adjusted segment-size policy.
- Codec changes between recording sessions require renegotiation; playback stops
  on a codec change. A player opened without an active recording does not subscribe
  to recordings started later. General source reconnect/gap policy remains work.
- Arrival-time PTS/DTS and the other original WSC-RTP/live-transition findings below
  remain separate issues.

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
