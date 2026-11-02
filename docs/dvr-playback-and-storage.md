# DVR playback and storage behavior

## Playback lifecycle

Playback stays in DVR mode when it reaches the end of a finite recording. Its
state becomes `ended`; the client can seek again or explicitly return to live.
At the edge of an active recording, playback reports `waiting`, resumes when frames
arrive, and reports `error` if nothing becomes available for thirty seconds.

A seek into a recording gap selects the next available recording. Playback skips
media gaps greater than one second to the next independent frame and reports
`gap_skipped`. The media clock is rebased so viewers do not wait through the gap.
Starting at a decoding anchor can also start playback earlier than the requested
seek time. The returned time and mode notification describe the actual start.
Runtime read/catalog errors stop playback and report `error`; a failed explicit
seek preserves the existing playback.

`GET /client-session-control/{id}/mode` includes an optional `playback` object with
one of `playing`, `waiting`, `gap_skipped`, `ended`, `error`, or
`renegotiation_required`. `current_time_ms` remains the absolute Unix timestamp of
the current DVR frame. The `playback` object records the latest state transition.

WSC-RTP clients can opt in to structured notifications:

```text
/streams/{source_id}/wsc-rtp?playback_events=true
```

Example:

```json
{"type":"playback","event":{"state":"ended","timestamp":1793283470689}}
```

Without this opt-in, older clients retain their existing message types; fatal
playback/renegotiation failures use the existing `error` message. REST polling still
exposes the structured state. The Rust RTP example opts in and prints events.

## Codec changes and speed

WSC-RTP updates SDP before sending media with new codec parameters. Independent
frames establish each new timeline; stale queued media is invalidated. Both codec
changes and same-codec parameter changes are handled. RTSP packetizers can be
reinitialized when a source restarts with a different codec.

A WebRTC track is negotiated for a fixed codec. Incompatible playback requests
return an explicit renegotiation error, and an incompatible automatic transition
stops with `renegotiation_required`. Submit a new offer/session supporting the
required codec. `POST /streams/{source_id}/webrtc/dvr` negotiates using the selected
recording's codec, not the current live stream's codec. This implementation does not
exchange unsolicited offers or replace a negotiated track in place.

Forward DVR playback supports finite rates from `0.25` through `4.0` via
`POST /client-session-control/{id}/speed` with `{"speed": 2.0}`. Zero, negative,
non-finite and out-of-range rates fail with HTTP 400. Speed changes preserve the
current decode sequence; DVR seeks preserve the selected speed. Live mode is 1x.

Accelerated DVR playback automatically returns to live/1x when it comes within
100 ms of a fresh active recording head. Finite recordings still end in DVR mode;
an offline source does not trigger catch-up. The live handoff waits for an
independent frame. WebRTC requires a compatible negotiated live codec.

WSC-RTP clients receive the existing `session_mode` live notification even without
opting into playback events. WebRTC clients can subscribe to
`GET /client-session-control/{id}/events` (SSE). Each `session_mode` event contains
`is_live`, `speed`, `current_time_ms` and `reason`; automatic catch-up uses
`"reason": "caught_up"`. A successful live handoff reports `is_live: true`,
`speed: 1.0` and `current_time_ms: null` before live media becomes eligible.

```javascript
const events = new EventSource(`/client-session-control/${sessionId}/events`);
events.addEventListener("session_mode", ({ data }) => {
  const mode = JSON.parse(data);
  updateLiveIndicator(mode.is_live);
  updateSpeedSelector(mode.speed);
});
// Close events when the viewer/session is disposed.
```

SSE sends the latest mode/speed snapshot immediately on connection and reconnect.
It coalesces updates for slow clients without blocking media delivery; consumers
should treat events as authoritative state rather than a history of transitions.
The snapshot timestamp is the position at the latest control transition; use
`GET /client-session-control/{id}/mode` for the current DVR frame position.

## Crash recovery

The recorder creates a `.pending` descriptor for each new segment. On successful
finalization its `.json` catalog is published and the descriptor is removed. On
startup, before sources or HTTP requests are accepted, recovery processes pending
segments: it discards incomplete trailing MP4 boxes, verifies complete fragments
and an independent decoding start, rebuilds the end timestamp and publishes the
catalog. The journal's original wall-clock mapping and codec are retained.

If recovery cannot obtain decodable media, the file and descriptor are retained
under `.mp4.unrecoverable` and `.pending.unrecoverable` names. Permission failures
are logged and the original files are retained if quarantine cannot be performed.
These files are excluded from playback and automatic retention for manual review.
A completed catalog left alongside a pending descriptor is treated idempotently.
This is process-crash recovery; power-loss durability is not guaranteed. Files from
before pending descriptors were introduced cannot be reliably timed and are not
converted automatically.

## Automatic retention

Pass a positive `--dvr-retention-secs` to retain that much recording history. For
example, `media-server --dvr-retention-secs 604800` keeps seven days. Omitted or `0`
disables deletion; malformed values fail startup. There is no arbitrary default
deletion period. The old environment variable is not read. See the README for all
startup arguments.

Cleanup runs every sixty seconds by default, starting immediately. Set a different
positive interval with `--retention-check-interval-secs`. It deletes completed,
cataloged segments whose end time is older than the cutoff. Active files, pending
segments and files held open by a playback reader are protected. Expired future
segments that a viewer has not opened can be removed; playback then follows the
gap/end policy above. Quarantined and unindexed files are never silently deleted.

The DVR root defaults to `dvr`; `--dvr-dir` changes it consistently for recording,
playback, recovery and retention. Paths are relative to the working directory.

## In-memory recording catalog

After recovery, completed segment metadata is loaded into a process-local catalog.
Playback reuses cached directory membership and segment metadata instead of
rescanning directories, opening JSON files and checking every MP4 on each lookup.
Finalization publishes the JSON marker and updates the catalog under the same
write lock; retention withdraws both and restores both if media deletion fails.
New recording sessions are registered as their segments finalize. Active recording
files still use the existing live session state and readiness checks.

JSON and pending descriptors remain the persistent format; no storage migration is
needed. Changes made outside the server (including editing or deleting sidecars)
require a restart to refresh metadata. An unreadable catalog is logged during
startup and still fails playback lookup for that stream. Catalog memory grows
with the number of retained segments. Lookups still clone and sort full metadata
snapshots; this change removes repeated filesystem work, rather than introducing
a binary-search seek API. Retention still enumerates recording directories and
checks deletion protections, but reuses cached segment metadata.

Reproduce the catalog benchmark with:

```sh
cargo test -p media-server --release benchmark_catalog_lookup -- --ignored --nocapture
```

The benchmark compares the previous directory scan/JSON parse/MP4 existence check
with the cached completed-catalog snapshot, verifies matching entries and ordering,
and reports initial load time plus p50/p95 lookup latency. Each size uses 50 paired
lookups with warm filesystem caches, across sessions containing up to 1,000
segments each. Tiny placeholder MP4 files are sufficient because neither path
decodes media. Results measure metadata lookup only, excluding active recordings,
video decoding, end-to-end seek latency, and finalization/retention throughput.

Measured locally on 2026-11-02, Intel Core i7-12700KF, Linux/Btrfs,
Rust 1.97.0, release profile (times in milliseconds):

| Segments | Sessions | Initial load | Scan p50 | Cached p50 | Scan p95 | Cached p95 | Median speedup |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 100 | 1 | 0.408 | 0.180 | 0.00576 | 0.210 | 0.00767 | 31.3x |
| 1,000 | 1 | 2.676 | 1.871 | 0.05693 | 1.965 | 0.05945 | 32.9x |
| 10,000 | 10 | 28.531 | 26.257 | 0.75074 | 27.628 | 0.84569 | 35.0x |

These are single-process microbenchmark results, not a production throughput
claim. Both paths benefit from warm OS caches; results vary with filesystem,
hardware, catalog size and concurrent server activity.
