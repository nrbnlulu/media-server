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

Only speed `1.0` is supported. Other rates, including zero, negative, NaN and infinite
values, fail with an unsupported-speed error (HTTP 400 on the REST speed endpoint).

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
