# Playback-only sessions for imported pre-alarm clips

Status: implemented and deployed to the local Compose media-server on 2026-11-05.

Implementation uses an optional live packetizer and immutable clip bounds on the
existing session, with readiness serialized by its state lock. No synthetic live
source, wire mode, client API or player change was needed. The completed-clip
lookup is shared with import existence checks and rejects staging media.

Validation: workspace tests pass (91 media-server tests, 2 existing ignores,
plus the RTP crate test), strict Clippy and formatting checks pass. New tests
cover H.264/H.265 sessions, catalog reload, readiness, bounded/early seeks, EOF,
replay, invalid clips, cleanup, actual WSC WebSocket/UDP/fallback negotiation and
a connected local WebRTC peer. Deployed incident #694 delivered 2,731 RTP packets,
reached EOF, accepted a tail seek and replayed from the beginning. Visual Flutter
verification remains manual; endpoint playback alone does not establish it.

## Goal and acceptance

Open a completed `prealarm:*` clip through the existing WSC-RTP and WebRTC APIs,
without registering an RTSP source. The current Flutter player and media-server
SDK remain unchanged. Existing live streams and live/DVR switching retain their
behavior. Authentication is the final project stage; keep current import-token
protection in place.

The immediate acceptance case is incident #694 at מאגר הלל: open its existing
clip in the UI, decode video, seek, reach the end, replay, and close cleanly. No
new alert or reimport should be needed if retention has preserved the clip.

## Confirmed gap

- `app.rs::create_wsc_rtp_registration` requires `GlobalState.sources` and creates
  a live session. `api/handlers.rs::wsc_rtp` independently requires `get_source`.
- `create_rtc_session` requires a live packetizer even when DVR time is supplied.
- `seek_session` rejects any session whose source is absent from the live map.
- `ClientSession` requires a live packetizer; switching and deletion assume one.
- WSC initialization only needs the source for `active_input()`. That value is
  already optional, so a clip can send `active_source: null` with the same schema.
- `SegmentedPlayer` already loads codecs from disk, prepares without playing,
  seeks through the root-aware catalog, and emits finite `PlaybackEvent::Ended`.
  Existing domain tests cover this, but do not exercise session signaling.
- WSC installs RTP/control channels after SDP and UDP negotiation. Starting a
  finite clip during registration would discard opening frames before transport
  is ready. WebRTC has the equivalent risk before the peer connects.

## Design decisions

### 1. Resolve a session source without inventing a live source

Add a small internal source descriptor shared by both registration paths:

- `Live`: existing live packetizer and optional active input.
- `Clip`: source ID plus validated actual start/end from the published catalog.

Resolve reserved `prealarm:*` IDs exclusively from the published pre-alarm root;
do not insert dummy sources into `GlobalState.sources` or create recorders.
Unknown ordinary IDs retain current not-found behavior. General playback of
orphaned live DVR recordings is outside this stage.

Reuse/refactor `import_jobs::completed_in` and `result_from_segments` into one
read-only completed-clip lookup used by both existence checks and registration.
Do not depend on an in-memory import job or SQL metadata: clips must work after
restart. Exclude staging and empty/incomplete media. Obtain codec parameters
through `DvrPlayer::codec_parameters` at the selected start time. Keep filesystem
and decoding work off the async executor where existing helpers do so.

Missing/expired clips return not-found. Unreadable or corrupt published media
returns a meaningful preparation error before session registration completes.
Do not report every failure as an unknown live stream.

### 2. Represent capability separately from current playback mode

Replace the mandatory live packetizer in `ClientSession` with an internal input
variant: live-capable or finite clip with bounds. Keep the existing `Live`/`Dvr`
wire modes; a prepared clip is DVR even before its transport starts.

Construct a clip session with a prepared `DvrPlayer` feeding the existing
`StitchingConsumer`, initially at its first available decoding point. Preparation
must not start the paced playback task. Track transport readiness and pending
start/seek under the session lock; readiness starts the latest prepared position
exactly once. Avoid adding a public pending mode or requiring a client command.

Remove the live-map prerequisite from `seek_session`. Dispatch through the
session's own capabilities. Return an existing API error for `switch_to_live`
on a clip, leaving its current playback untouched. Speed remains 1.0 only.

### 3. WSC-RTP startup

1. Resolve and prepare the clip before publishing its session registration.
2. Seed the publisher's codec parameters so initial SDP does not require frames.
3. Pass optional active-input metadata to the WebSocket handler, rather than a
   required `VideoSource` object. Live sources provide their current value;
   clips provide `None`.
4. Preserve the existing Init, SDP, UDP hole punch and WebSocket fallback protocol.
5. Install RTP/control channels and enqueue any required fallback message.
6. Notify the session that transport is ready, enqueue DVR mode, then begin
   playback. Ensure mode/control ordering precedes the first eligible RTP frame.

Serialize early REST seeks with readiness. A seek before readiness updates the
prepared position without emitting frames; readiness must not overwrite that
position with clip start. A seek after readiness uses the existing generation
and stitching mechanisms to discard stale queued packets. Refactor preparation
and start in `SegmentedPlayer` only as needed; do not duplicate its seek reader.

Retain `playback_events` opt-in. Existing clients get the same protocol messages
as today; newer clients can receive finite playback events. Verify the pinned
player accepts null active input and initial DVR mode, without editing it.

### 4. WebRTC startup

Use the same resolver and session construction for both ordinary WebRTC creation
and the explicit DVR WebRTC route. A clip defaults to its start; an explicit
DVR timestamp selects the initial position. Build SDP from stored codec parameters.

Start finite playback only when the peer is connected, after the session has
been registered. Use a readiness latch or equivalent state check so a connection
callback racing with insertion cannot lose the start notification. Serialize
connection readiness and early seeks as with WSC. Preserve live startup behavior.
Validate codec compatibility during negotiation and clean up a rejected offer.

### 5. Seeking, end, and reconnect

- Use server catalog bounds, already expressed in our reference timeline. Apply
  no device-clock offset during playback; import already performed conversion.
- Clamp clip seeks to its actual start/end. Seeking to the inclusive end selects
  the last decodable GOP and naturally reaches Ended. Report the actual keyframe
  position using existing mode/status fields; do not promise frame-exact seeking.
- Reuse existing gap handling inside the clip and cover exact boundary behavior
  with fixtures. Do not alter live DVR seek semantics.
- At EOF, keep the session in DVR with Ended status. Do not switch to live, wait
  for future media, automatically loop, or close the transport just for EOF.
- Seeking after EOF restarts playback through the same session.
- Reconnection creates a new session at clip start; the existing UI may restore
  position with its normal seek call. Do not add server-side resume storage.
- If retention removes media during preparation or playback, fail cleanly using
  existing error/status paths. This stage does not add retention pinning.

### 6. Ownership and cleanup

Registration is transactional: resolve and prepare before inserting map entries;
roll back publisher, peer, player and consumers on later setup failure.
Handle WebSocket upgrade failure as well as normal disconnect, negotiation timeout,
explicit deletion and server shutdown. Cleanup may race and must be idempotent.
A clip has no live consumer to detach and must not log a missing-live-source error.
Release map guards before network operations or long awaits where paths are touched.
Do not leave playback tasks running after transport teardown.

## Implementation sequence

1. Shared completed-clip lookup and capability descriptor, with lookup tests.
2. Prepared playback-only `ClientSession`, seek/readiness/end/cleanup behavior.
3. WSC handler integration and real WebSocket handshake/packet tests.
4. WebRTC integration using the shared session path and connection-ready start.
5. Workspace checks, rebuild, recreate the local Compose media-server with its
   existing binary/token override, and verify incident #694 through the UI.

Primary files: `src/app.rs`, `src/api/handlers.rs`,
`src/publishers/wsc_rtp.rs`, `src/publishers/webrtc.rs`,
`src/domain/dvr/import_jobs.rs`; change `src/sources/segmented_dvr.rs` only for
necessary prepare/start coordination. Paths are relative to the media-server crate.

## Validation

Use synthetic repository fixtures, not captured site footage, for automated tests.

- Resolve completed H.264/H.265 clips with no live source, including after catalog
  recovery; reject unknown, expired, staging, empty and corrupt clips.
- Exercise the actual WSC upgrade route: Init with null active input, stored-codec
  SDP, initial DVR mode, decodable opening GOP, paced RTP and finite completion.
  Cover forced WebSocket, successful UDP and UDP timeout/fallback. Delay readiness
  long enough to prove a short clip does not advance or lose its opening frames.
- Test before-start/at-start/midpoint/at-end/after-end seeks, seek before readiness,
  repeated seeks, replay after EOF and rejection of go-live. Assert actual positions
  and stale-generation exclusion rather than merely successful HTTP responses.
- Test legacy playback-events-disabled and opt-in sessions. Verify mode polling
  exposes DVR/Ended even when the client does not subscribe to playback events.
- Establish a local WebRTC peer, consume RTP and reach EOF without RTSP. Exercise
  explicit timestamp creation, early seek, negotiation rejection and cleanup.
  Codec-specific negotiation failures must be clear; WSC covers both video codecs.
- Exercise concurrent clients, aborted upgrade, disconnect during negotiation,
  repeated cleanup and deletion during playback. Assert no leaked map entries,
  player tasks or live consumers. Test retention deletion as a controlled error.
- Retain live session, live-to-DVR-to-live, RTP continuity and codec regression tests.

Run `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`
and `cargo fmt --all -- --check`. Then build and recreate the running service.
In the UI verify #694 opening video, middle seek, EOF, replay and reopening; record
any client behavior that cannot be confirmed automatically. Do not declare UI
playback verified from a successful import or WebSocket upgrade alone.

Authentication, backend retention-row synchronization, generic ONVIF replay,
player/SDK changes and unrelated UI issues remain separate work.

## Shared Gantt cursor integration

The application now combines pre-alarm clips with regular playback sources in
one grid. Unpinned clip tiles follow manual cursor seeks and loop jumps and only open
players once the cursor enters their actual bounds. Pins retain tiles outside
the cursor window and enable independent bounded seeking/replay without moving
the Gantt cursor or other videos. Unpinning restores cursor following. Expanded
pinned views and standalone history previews retain local controls.

Native `recording_start_time` now considers the earliest retained DVR segment as
well as the active recorder start. The UI reads this existing field before
opening a regular DVR player, so a cursor window overlapping the first alert
does not play video before the cursor reaches actual media. Later alerts can
seek into earlier recordings, including after recorder restart. The added
regression test covers retained footage versus newer active recorder time. The
configured site-7 NVR source was checked through video-service and reported its
retained DVR start after deployment; clip #694 still accepts DVR timestamp seeks.
