# Long/open GOP and high-bitrate DVR checks

Baseline: 7fc4168 (segmented migration already committed).

Use generated moving H.264/H.265 video through the actual recorder. Check long
closed GOPs below and above the recent-buffer duration; open GOPs with B frames;
and CBR streams that produce segments above the playback memory limit. Inspect
live-buffer availability, finalized-file durations/sizes, seek preparation, frame
counts and decoded frame hashes. Preserve commands and measured results in
REVIEW_FINDINGS.md. Do not change production camera settings or recordings.


Completed November 1. Ten generated cases were recorded and decoded. Reproducer:
`python3 media-server/tests/camera_compatibility_probe.py` (exit 1 currently reports
confirmed incompatibilities). The full findings are in `REVIEW_FINDINGS.md`.

Confirmed: long-GOP recent/history gaps; oversized-segment playback failure at
~80 Mbps; oversized active GOP eviction; open-GOP independent-start frame loss;
and arrival-time PTS=DTS warnings with B frames. Continuous elementary-stream decode
preserves all open-GOP source frame hashes, so this is distinguished from seek/start
safety. The ~40 Mbps short/closed-GOP case passed the measured checks.

Production playback code and camera settings were not changed. The optional probe
and Python runner are left uncommitted for review. Normal tests: 37 passed, two
optional probes ignored. Clippy, formatting and whitespace checks pass.
