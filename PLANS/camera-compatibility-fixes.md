# Camera compatibility implementation

Authorized scope: preserve source PTS/DTS, bounded incremental playback, readable
active fragmented recordings, disk-aware eviction, dependency-aware seeking, and
compatibility regression validation.

First implemented change: duration-based recent-buffer eviction is a soft target.
The active GOP survives beyond thirty seconds when within the hard byte budget.
A regression test covers a forty-five-second GOP; byte-budget eviction remains
covered separately.

Remaining implementation: source timestamp mapping and DTS pacing; incremental
segment demux; active-fragment reader prototype; disk-aware GOP retention; codec-safe
seek anchors and preroll. The first change alone does not fix historical gaps when
a following keyframe arrives before the previous segment is finalized, oversized
GOPs, or open-GOP seeks. No claim of full compatibility is made.


Continuation: source packets now retain PTS/DTS differences using the source
FFmpeg timebase. Frame identities and pacing use DTS, while outgoing RTP timestamps
and current playback time use PTS. Missing source timing retains the arrival-time
fallback. A regression test exercises the actual FfmpegConsumer packet entry point.

Production disk playback now owns a persistent SegmentReader. Its appsink is bounded
by bytes/buffers and it reads batches limited to 4 MiB or 64 frames (plus the final
access unit). Whole-file collection exists only in test/probe code. Seek preparation
still retains a bounded GOP/preroll, so a single oversized GOP remains unsupported.

Validated: 39 normal tests passed, two probes ignored. Closed-GOP B-frame probes for
both codecs and ~80 Mbps H.264 pass decoded hashes, sampled seeks and timestamp
checks. Clippy/formatting/whitespace checks pass. Active-file reading, disk-aware
retention and open-GOP dependency-aware seek are still pending. No deployment.
