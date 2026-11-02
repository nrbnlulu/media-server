# DVR playback speed support

## Implementation status

Implemented the shared rate/clock, seek persistence, signal-driven catch-up
controller, live/1x handoff, existing WSC-RTP mode notification, and SSE state
notifications. SSE uses a Tokio watch channel: consumers receive the latest state
and updates may coalesce. This replaces the proposed revision/history approach;
the subscription itself atomically retains the current snapshot and future state.
Native tests cover clock scaling, rate changes, original 1x timestamp mapping,
accelerated RTP output, seek persistence, active-head catch-up requests, finite
recordings, and handoff notifications/reconnect snapshots. Workspace tests pass.

Still pending: browser/receiver validation, variable-frame-rate tuning of the
100 ms threshold, measured end-to-end notification/media ordering, and 4x load
testing. The initial 0.25–4x range is implemented but not receiver-certified.

## Scope and proposed behavior

Implement per-session forward video playback at finite rates from 0.25x through
4x, inclusive, with 1x as the default. This range is a proposed initial product
limit, subject to receiver validation; do not promise higher rates without testing.
Reuse the existing REST speed endpoint for both WebRTC and WSC-RTP sessions.
No recording format changes, transcoding, audio time stretching, reverse playback,
pause (speed zero), frame skipping, or client UI work in this phase.

- Reject non-finite, zero, negative, and out-of-range rates before changing state.
- Speed remains DVR-only; live sessions retain the existing rejection behavior.
- Apply changes at an access-unit boundary without seeking, restarting decoding,
  clearing media queues, or waiting for a keyframe. Already transmitted media
  cannot be recalled, so receiver buffering may delay the visible effect.
- Preserve the selected rate across successful DVR seeks and gap/codec transitions.
  Failed seeks preserve both the previous playback and its rate.
- Returning to live resets the effective rate to 1x; entering DVR again starts at 1x.
- When accelerated playback (rate > 1x) catches the active live stream, switch
  automatically to live at 1x and notify the client. Ordinary 1x/slow playback
  retains its existing DVR behavior. Finite recordings still end in DVR mode.
  Missing media or an offline/stalled source must not trigger a live switch.
- A speed change on an ended/error player updates its selected rate for the next
  successful seek but does not restart playback or clear the terminal event.
- `current_time_ms` and playback-event timestamps remain original Unix media times,
  independent of the transport clock. Mode polling reports the selected rate.

## Existing implementation

- `media-server/src/sources/segmented_dvr.rs`: `SegmentedPlayer::speed()` is fixed
  at 1.0 and `set_speed()` rejects other values. `stream()` paces by absolute DTS
  deltas and packetizes absolute PTS into a 90 kHz RTP timestamp. It already has
  bounded batch lookahead, gap handling, codec changes and active-edge waiting.
- `media-server/src/app.rs`: `set_session_speed()` forwards DVR requests;
  `ClientSession::seek()` explicitly passes 1.0 to `seek_to_timestamp()`.
- `crates/media-server-api-models/src/lib.rs`: `SpeedRequest` and the `speed` field
  in `SessionModeResponse` already exist. No new request shape is required.
- `media-server/src/api/handlers.rs`: the existing speed handler maps failures to
  HTTP errors. Preserve the public route and document validation errors.
- `media-server/src/common/rtp.rs`: `RtpStitcher` preserves output continuity on
  explicit source switches, currently using a fixed 3000-tick transition step.
  Ordinary speed changes must not trigger that switch mechanism.

## Implementation

### 1. Validated rate and playback clock

Introduce a small internal validated rate type and a pure playback-clock helper
near the DVR player. Keep floating-point input at the API boundary, use relative
times for scaling, and use checked/signed wide arithmetic where appropriate.
Never divide an absolute Unix timestamp by the rate.

The clock maintains a source DTS anchor, monotonic wall-clock anchor, and output
media-time anchor. Within a constant-rate interval:

```
send_deadline = wall_anchor + (frame.dts - source_anchor) / rate
output_pts   = output_anchor + (frame.pts - source_anchor) / rate
rtp_ts       = wrap_u32(output_pts * 90_000 / 1_000_000_000)
```

Scale both scheduling and RTP presentation intervals. Changing only send pacing
would leave the receiver's presentation clock at the original speed. Preserve
signed PTS-minus-DTS composition offsets for reordered frames; RTP timestamps need
not be monotonic in decode order. Retain fractional precision across frames to
avoid cumulative rounding drift and test timestamp wrapping.

On a running rate change, evaluate the current virtual source position under the
old clock and re-anchor there before applying the new slope. Recompute the pending
frame deadline instead of retaining a sleep calculated with the old speed. Keep
transport time continuous. Do not extrapolate the source clock through periods
where playback is waiting for media or substantially stalled by downstream work.
Define bounded lateness handling by rebasing the clock after a stall rather than
emitting every overdue frame immediately; preserve all encoded dependencies.

### 2. Dynamic control and lifecycle

Store the validated selected rate in the player and publish updates through a
Tokio watch channel. A speed request acknowledges a validated selected rate; the
playback task observes the latest value at the next safe frame boundary. This
allows rapid requests to coalesce and avoids requiring a response from an ended
task. Same-rate requests are no-ops.

Use `select!` around pacing sleeps and rate notifications. Retain the pending frame
while recomputing its deadline. Changes must also remain observable during edge
waiting and prefetch waits without starting duplicate readers or abandoning the
existing prefetch cancellation guard. Do not hold synchronous locks over awaits.

Separate seek validation from mutation: remove the mutating speed call at the
start of `seek_to_timestamp()`, and preferably remove its speed argument entirely.
Prepare and codec-check the new position first, then replace the task using the
player's selected rate under the existing session/state serialization. Initialize
each new task from the latest watch value, including updates made after it ended.

Re-anchor on gap skips and on resumption after actual media starvation. Retain
the current lifecycle events and thirty-second wall-time timeout. Test codec and
parameter changes with the scaled clock while preserving the existing queue
invalidation and independent-frame requirements.

### 3. Automatic catch-up transition and client notification

Detect catch-up against the active source's latest media position, not wall-clock
Unix time or merely an empty disk batch. Compare timestamps in the same source
timeline using DTS (presentation order may differ with B-frames). Initially use
a 100 ms proximity threshold, to be validated with variable frame rates. Require
an active, advancing live source and rate > 1x. Do not mistake recording gaps,
disk/read delays, a closed recording, or a stale active session for catch-up.

The player sends an internal catch-up request to the session controller. Include
a playback generation so a delayed request cannot override a newer seek, speed
change, manual switch or session teardown. Recheck eligibility under session
serialization. Do not call `switch_to_live()` from inside the playback task:
the existing method aborts and awaits that task and would risk self-cancellation.
Use a controller-owned task with a weak session/global-state reference and no
DashMap guard held over awaits; clean it up on session termination.

Reuse the existing live handoff and stitching machinery, including stale-queue
invalidation and independent-frame gating. Check live codec compatibility before
committing. An incompatible WebRTC codec remains DVR with
`renegotiation_required`; never announce a successful live transition. When live
is unavailable, keep DVR waiting under the existing timeout policy. A compatible
handoff must begin at a decodable live frame; validate the existing gating and
ensure a failed preparation does not discard the working DVR player.

Commit the mode and effective speed together as live/1x. Queue the notification
before making live media eligible, without exposing an intermediate mode/speed
combination to REST readers. Send one logical transition per catch-up; do not
emit a successful transition merely because catch-up was requested.

Notification delivery must cover both transports:

- WSC-RTP: send the existing `SessionMode::Live` control message, including to
  clients without `playback_events=true`. Its contract explicitly means speed
  is now 1x, allowing existing clients to update their live indicator and speed
  selector without requiring a new message variant.
- WebRTC: `on_session_mode_change()` currently only clears internal playback
  state and does not notify the remote client. Add a session-scoped SSE endpoint,
  `GET /client-session-control/{session_id}/events`, for mode transitions, usable
  by either transport. Send a `session_mode` event with `is_live: true`,
  `speed: 1.0`, `current_time_ms: null`, and `reason: "caught_up"`. Manual
  transitions use their corresponding reason. Use the same session access rules
  as existing controls. Do not claim REST polling alone fulfills notification.
- Send the authoritative current mode/speed snapshot on SSE connection and
  reconnection so missed events cannot leave clients displaying DVR/4x forever.
  Subscribe before taking the snapshot and order/deduplicate updates with a
  session revision. Bound subscriber queues and resynchronize lagging consumers;
  a slow/disconnected notification client must not block media playback.
- REST mode polling remains authoritative and returns live/1x with no stale DVR
  playback event after a successful handoff. Document client subscription and
  reconnect behavior; include a receiver example consuming the notification.

### 4. Transport and API integration

Keep speed scaling in the shared DVR source so both publishers receive the same
retimed RTP timeline. Keep RTP clock-rate declarations at 90 kHz. Check the complete
publisher path for any timestamp rewriting or RTCP sender-report assumptions
that could undo this mapping; verify sender timing in receiver integration tests.
Existing seek/source-switch stitching can remain initially, provided continuity
tests demonstrate correctness; do not broaden this work into a stitcher rewrite.

Keep `SpeedRequest` and `SessionModeResponse` compatible. Update their comments,
OpenAPI endpoint descriptions, and `docs/dvr-playback-and-storage.md` to explain
the accepted range, seek persistence, automatic catch-up to live/1x, notification
contract, and the absence of pause support. Existing clients continue at 1x.
Retain existing WSC-RTP message shapes; add the shared SSE event model and route
to API models, handlers, router and OpenAPI documentation.

## Verification and acceptance

1. Pure clock tests: 0.25x, 0.5x, 1x, 2x, 4x; exact interval scaling, dynamic changes
   between frames, repeated changes, large source timestamps, fractional rates,
   RTP wraparound and PTS/DTS reordering. At 30 fps, 2x should produce about 16.67 ms
   send spacing and 1500 RTP ticks; 0.5x about 66.67 ms and 6000 ticks.
2. Deterministic Tokio-time tests with a recording consumer: interrupt a pending
   slow-rate sleep, preserve pending frames and sequence continuity, preserve rate
   on seek, reject invalid input without mutations, and isolate two sessions.
   Enable Tokio's test-time utilities as a dev feature if needed.
3. Lifecycle tests: failed seek, seek after end, rate changes while waiting/ended,
   gap skip, active edge resume, finite end, codec incompatibility and return to
   live. Assert no unbounded overdue-frame burst after starvation or backpressure.
   Test 4x catch-up switching exactly once to live/1x, while 1x/slow playback,
   offline sources, read delays and finite recordings do not auto-switch. Cover
   races with seeks/rate changes/teardown, incompatible live codecs, and first
   live-frame decodability. Verify WSC-RTP notification precedes eligible live
   media, SSE delivers the catch-up reason and live/1x state, reconnect snapshots
   recover missed events, and notification backpressure cannot stall playback.
4. Transport tests: all packets in an access unit retain the same timestamp;
   payloads, markers and frame order are preserved. Test scaled timestamps through
   the actual stitcher and WSC-RTP queue, including UDP-to-WebSocket fallback.
5. Receiver validation: play a recording with visible timecode in a WebRTC browser
   and the WSC-RTP example/client at each supported rate, including mid-GOP changes
   and B-frame material. Over a sufficiently long steady interval, source-time
   advance should track selected rate within 5%, allowing startup buffering.
   Check that RTCP/jitter buffering does not restore 1x or stall decoding.
6. Run formatting, relevant focused tests and the workspace tests. Document native
   FFmpeg/GStreamer prerequisites or environmental failures. Compare 1x behavior
   against the existing fixture and assess CPU, queue growth and bandwidth at 4x
   with multiple viewers. Full-frame 4x playback can require roughly 4x bitrate and
   decode throughput; lower the advertised ceiling if representative receivers
   cannot sustain it. Do not silently drop dependent frames to meet the target.

## Delivery order

Implement and test the rate/clock helper first, integrate dynamic control and seek
semantics second, then verify publishers and API behavior. Finish with receiver
validation and documentation before declaring the supported range ready. No
storage migration or new production dependency is expected.
