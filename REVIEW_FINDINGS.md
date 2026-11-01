# WSC-RTP and DVR/live review

Reviewed October 28–29, 2026. Source revision: `ad1814a`.

The review combined code inspection, the existing test suite, packet probes against
`http://localhost:8000/media`, and playback on the connected Armor 21 Android device.
The deployed server's exact source revision was not established. During the initial
review, no implementation changes were made. All 25 existing tests passed; they do not cover WSC-RTP
negotiation or DVR/live transitions.

## Safe DVR/live switching and heartbeat fixes

Implemented generation-scoped playback inputs and complete-access-unit delivery.
A source switch waits for in-flight frame delivery, invalidates the previous input,
and gates the new live input until a complete independent H.264 IDR / H.265 IDR/BLA
frame arrives. The live packetizer prepends cached SPS/PPS or VPS/SPS/PPS and only
marks a frame eligible for startup once those parameters are available. Old cloned
live subscriptions cannot feed a newly selected timeline. Failed DVR preparation
restores live through a fresh gated input. The live mode notification is enqueued
before subscribing to new live media.

WSC-RTP queues carry their source generation; obsolete media and mode notifications
are discarded before transport writes. The WebSocket writer checks again after its
bounded output queue. Sequence numbers/timestamps are stitched at the transport
boundary, using packets actually selected for output rather than obsolete backlog.
WebRTC retains the common generation/frame gate and stitching path.

WebSocket receiving now runs independently throughout SDP and UDP negotiation.
JSON pings and standard WebSocket Ping/Pong are handled during startup; the five-
second receive inactivity timeout uses Tokio's monotonic clock. EOF and Close end
the receiver. Connection exit cancels the reader/writer tasks. Writes have a five-
second deadline, so a stalled socket cannot retain connection tasks indefinitely.
The initial SDP also initializes deduplication, avoiding an identical first update.

Validation: 41 tests pass, one optional probe ignored; Clippy with warnings denied,
formatting and whitespace checks pass. Regression tests cover stale generations,
independent-frame gating, switching during fragmented delivery, parameter sets and
fragmented H.264/H.265 IDRs, writer-side stale-queue filtering and output continuity,
startup heartbeats while negotiation is pending, idle timeout and EOF. Tests use
controlled streams/sinks; this change has not been deployed or checked on a camera
or real UDP/WebSocket network during this stage.

Remaining limits: already transmitted packets cannot be recalled, and UDP may
reorder them. The RTP producer queue is still unbounded; general overload policy
remains separate work. A camera supplying no supported independent decoding point
will leave live output gated. General codec renegotiation, client transition tagging,
credential sanitization and the other unresolved findings remain open. The historical
entries below describe the original behavior. Changes are uncommitted for review.

## Legacy playback removed

Playback now uses the segmented player directly. The legacy single-file player,
backend-selection wrapper, virtual MP4 index and legacy recording filename/lookup
helpers have been removed. Old loose MP4 recordings are no longer discovered or
played. Recording data on disk was not deleted. Historical review entries below
retain the context of the earlier implementation.

## Completed compatibility fixes — November 1

The implementation now passes all ten generated compatibility cases: closed
20/45-second GOPs; closed/open H.264/H.265 with B frames; approximately 40/80 Mbps
H.264; and approximately 80 Mbps with ten-second GOPs. The following sections retain
the earlier findings/progress as history; this section describes the current code.

Source PTS/DTS are preserved, disk playback is bounded, and seek preroll no longer
requires keeping a whole GOP in memory. Independent decoding anchors are detected
from NAL types (H.264 IDR; H.265 IDR/BLA). Seeks search preceding files when needed,
stream from the selected anchor, and notify the client of the actual earlier time.

The recorder now writes fragmented MP4 directly and rotates at independent decoding
points after the nominal ten-second duration. The prototype established that merely
enabling fragmentation inside splitmuxsink was insufficient: its GOP lookahead
withheld output. Direct mp4mux plus unbuffered filesink makes internal fragments
readable during recording. Active-file metadata is discoverable; the demuxer sees
only bytes through the last complete mdat box. This byte snapshot adds no synthetic
index or footer. Playback refreshes it after readable growth. Recent-buffer eviction
retains dependency overlap until completed files are available, subject to the hard
memory budget; active fragments provide disk fallback after forced cache eviction.

Validation: 41 normal tests passed, two optional probes ignored; all ten compatibility
cases pass decoded frame hashes, sampled seek preparation and timestamp checks.
Clippy with warnings denied, formatting and whitespace checks pass. Additional tests
exclude a deliberately truncated final mdat and verify active-file playback across
RAM eviction and later appends, checking every emitted frame timestamp. These are
local generated-stream checks; the updated implementation was not deployed or
retested on a physical camera/native GUI during this stage.

Limits: a seek can visibly start much earlier, especially when IDR/BLA frames are
rare. Such cameras also produce longer files. The current finite growing-file
snapshots reopen and scan when exhausted, which may be costly for very long files;
this is bounded in memory, not constant-cost I/O. A single access unit is limited
to 64 MiB. If RAM is exhausted before its disk fragment is complete, that newest
footage cannot yet be sought. Exact-target hidden preroll requires client support.
Crash recovery and retention remain separate tasks. Implementation details are in
`PLANS/camera-compatibility-fixes.md`. Changes remain uncommitted for review.

## Compatibility implementation progress

The duration target now preserves a long active GOP within the byte budget.
Source PTS/DTS are preserved when available, and playback uses DTS for ordering and
pacing while retaining PTS for RTP and displayed recording time. Production disk
reads now use a persistent, bounded demuxer instead of collecting whole segments.
The whole-file collector is restricted to diagnostic/test code.

The closed-GOP B-frame cases now pass for both codecs without timestamp warnings;
the approximately 80 Mbps H.264 case now passes the recording/read checks and
sampled seeks. The normal suite has 39 passing tests and two ignored probes.
Active-file reads and open-GOP safe seeks remain unfinished. In particular, a
single seek-preroll GOP exceeding 64 MiB still fails explicitly. These results do
not yet establish complete camera compatibility. See `PLANS/camera-compatibility-fixes.md`.

## Camera compatibility checks — November 1

Baseline: `7fc4168` (the segmented migration is now committed). These checks use
locally generated moving H.264/H.265 streams through the actual recording pipeline;
physical camera settings and production recordings were not changed. GStreamer
1.28.8 and FFmpeg n9.0.2 were used. Playback/recording behavior is unchanged in this
check; the additions are a reproducible probe and these findings.

**Long/open GOPs and high bitrates cannot yet be marked supported.** The matrix
exposes failures that the initial short-GOP, low-bitrate tests did not exercise.

| Generated source | Measured result |
| --- | --- |
| H.264, closed 20-second GOPs, 62 seconds total | All 620 frames decode correctly after finalization, but temporary DVR history gaps appear while recording. At elapsed 31 seconds, rewinding 15 or 25 seconds fails. |
| H.264/H.265, closed 45-second GOPs, 92 seconds total | All 920 frames decode after finalization. The recent buffer becomes empty once the current GOP exceeds 30 seconds; even a one-second rewind fails at elapsed 31, 40 and 44 seconds. It fails again before the next keyframe at 90 seconds. |
| H.264/H.265, closed 2-second GOPs with B frames, 24 seconds total | Independently decoded files retain all 240 frames, but each file produces non-monotonic timestamp warnings. Preserving content alone does not establish correct B-frame timing. |
| H.264/H.265, open 2-second GOPs with B frames, 24 seconds total | Sequential elementary-stream playback retains all 240 frames and hashes match the source. Independently decoding the three MP4 files produces only 234 frames; H.264 also reports missing references. Seeking/starting at a file boundary is unsafe. Timestamp warnings occur here too. |
| H.264, closed 1-second GOPs, approximately 39.90 Mbps | All 240 decoded frame hashes match, sampled rewinds succeed, and completed files fit the playback budget. |
| H.264, closed 1-second GOPs, approximately 79.80 Mbps | Ten-second MP4 files are about 94.8–95.4 MiB. Both full segments fail with `DVR segment exceeds playback memory limit`. The recent buffer retains only about six seconds, so immediate rewind works while older seeks fail. The short final segment remains readable. |
| H.264, closed 10-second GOPs, approximately 79.80 Mbps | A single GOP exceeds the buffer budget. Recent video becomes empty before the next keyframe, and both full disk segments exceed the read budget. One-second rewinds already fail at elapsed 10 and 20 seconds. |

The bitrate cases use actual CBR filler NALs to reach the measured encoded rates;
these are byte-volume and storage/playback checks, not a high-resolution decoder
performance benchmark. All sources use 160×96 moving video at 10 fps. The recorder
receives access units in decode order with simulated arrival timestamps, reproducing
its existing PTS=DTS policy. The generator's original bitstream is retained as the
independent decoding reference.

### Why the failures happen

1. `Session::push` removes the oldest GOP once its first frame is more than thirty
   seconds old, even if it is the only retained GOP. It then discards delta frames
   until the next keyframe. Long-GOP footage becomes unavailable despite using very
   little memory. Even twenty-second GOPs can leave a hole: the recent buffer evicts
   footage before `splitmuxsink` has finalized the corresponding file. Rotation and
   finalization need subsequent keyframes/GOP data; ten seconds is not a hard bound.
2. `read_segment` collects the entire demuxed segment into a `Vec<Frame>` and rejects
   more than 64 MiB. With ten-second files, that corresponds to roughly 54 Mbps of
   encoded payload, and the effective bitrate limit is lower for longer files.
   `max-size-bytes` is currently disabled on the recorder. The separate 64 MiB
   recent-buffer limit can also remove the active GOP at high bitrates.
3. A keyframe flag does not guarantee an independently decodable closed GOP. The
   seek code selects flagged keyframes without retaining predecessor dependencies.
   The independent-file decode failure does **not** prove that uninterrupted RTP
   playback fails: concatenating the demuxed elementary streams and keeping decoder
   references preserved every frame and hash in both open-GOP cases. GStreamer
   explicitly requires closed GOPs for independent split-file playback:
   [splitmuxsink documentation](https://gstreamer.freedesktop.org/documentation/multifile/splitmuxsink.html).
4. The recorder replaces source presentation/decode timestamps with arrival time
   and assigns PTS=DTS. The closed-GOP B-frame controls isolate this existing timing
   defect from the additional open-GOP seek/dependency problem. Both need attention.

### Reproduction and follow-up

Run the complete matrix locally:

```sh
python3 media-server/tests/camera_compatibility_probe.py --output /tmp/dvr-camera-check
```

This requires the local Rust build dependencies, GStreamer `x264enc`/`x265enc`, and
FFmpeg. No network or device is required. The optional Rust probe records each
case, checks seek preparation during recording, and saves finalized MP4 files.
The Python runner compares independent and continuous decoding against the source
using frame hashes and checks timestamp warnings. It exits **1** when compatibility
failures are found; the Rust probe succeeding only means artifact generation worked.
Use `--decode-only` to recheck existing output, or `--cases` to select cases.

Artifacts are under `/tmp/dvr-camera-check/<case>/`: `results.json`,
`decode-results.json`, source bitstream and completed segments. The aggregate
report is `/tmp/dvr-camera-check/summary.json`. These generated video files are
not committed. The normal workspace suite passes: 37 tests passed, two optional
probes ignored. Clippy with warnings denied, formatting and whitespace checks pass.

Required fixes before claiming broader camera compatibility:

- Keep the GOP/dependency overlap needed for recent playback until disk footage is
  actually readable. A soft duration target must coexist with a hard byte budget;
  GOPs exceeding that budget need a disk-backed path or an explicit supported limit.
- Read large files incrementally with bounded queues rather than loading an entire
  segment. Byte-based rotation can reduce typical file sizes, but cannot split a
  single oversized GOP into independently playable pieces.
- Preserve source PTS/DTS and implement dependency-aware seek/preroll for open GOPs,
  or enforce/detect a closed-GOP recording requirement. Treating every flagged
  keyframe as an independent start point is insufficient.

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
