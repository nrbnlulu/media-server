# DVR Recorder: Use Wall-Clock Timestamps

## Context

When a camera has network issues or low bandwidth, frames arrive at the server in bursts. However, the PTS/DTS values on those frames reflect the **camera's encoding clock** — tightly spaced (e.g. 33ms apart at 30fps) regardless of when packets actually arrive. The DVR recorder writes these camera PTS values directly into the MP4. On playback, GStreamer's `sync=true` respects those compressed timestamps, so minutes of real-world time plays back in seconds ("fast-forward" effect).

**Goal:** Make DVR playback duration match real wall-clock duration by using wall-clock-based timestamps when writing to the MP4.

## Approach

Replace camera PTS/DTS with wall-clock-relative timestamps in `RecordingState::push_buffer_to_appsrc`. Instead of converting the camera's PTS to nanoseconds, compute `now() - recording_start_time` and use that as PTS/DTS.

This means:
- Frames that arrive in a burst get spread out to their real arrival times
- During network gaps, playback shows a frozen frame (the last frame before the gap) — which is exactly what a live viewer would have seen
- The MP4's internal duration matches the file name's wall-clock duration

## Changes

### File: `media-server/src/domain/dvr/recorder.rs`

1. **Add a `recording_start_instant` field to `RecordingState`** — a `std::time::Instant` captured when the recording state is created. This is used to compute wall-clock-relative PTS.

2. **Change `push_buffer_to_appsrc` signature** — remove `pts`, `dts`, `timebase` parameters. Instead, compute PTS/DTS internally as `Instant::now() - recording_start_instant` (in nanoseconds).

3. **Update `write_nal_data`** — remove `pts`, `dts`, `timebase` parameters (they're no longer needed since `push_buffer_to_appsrc` computes its own timestamps). Update all call sites within `write_nal_data`.

4. **Update `on_new_packet`** — stop passing `pts`, `dts`, `timebase` to `write_nal_data`. The packet's PTS/DTS are still read for other purposes (e.g. keyframe detection) but no longer used for MP4 timestamps.

### No other files change

The playback path (`dvr.rs`) reads timestamps from the MP4 and converts to RTP — it doesn't need changes since the MP4 will now contain correct wall-clock timestamps.

## Verification

1. `cargo build` — ensure it compiles
2. Manual test: connect a camera, start recording, simulate network issues (e.g. throttle bandwidth), play back the DVR recording and verify playback duration matches real elapsed time
