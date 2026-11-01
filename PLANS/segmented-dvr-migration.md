# Segmented DVR migration

Baseline: f49cbe8 preserves the recent-rewind fix and legacy recording playback.

New recordings use splitmuxsink with approximately ten-second, keyframe-aligned MP4 files. Only finalized files enter the persistent catalog. A shared, bounded encoded-frame buffer covers the active file and overlaps finalized segments. Playback uses one RTP packetizer and absolute media-time pacing across disk and memory; explicit seeks replace playback only after the target has been prepared. Existing single-file recordings remain readable.

Validation: generated video through the actual recorder, seek into disk and memory, playback across multiple file boundaries and disk-to-memory, buffer eviction, restart/catalog reload, repeated seeks, and legacy regression tests. No production recording migration or deployment.


Implemented and validated. See `REVIEW_FINDINGS.md` for details and limitations.

- New recorder: `media-server/src/domain/dvr/recorder.rs`.
- Persistent catalog and shared recent buffer: `media-server/src/domain/dvr/segments.rs`.
- Continuous encoded-frame playback: `media-server/src/sources/segmented_dvr.rs`.
- Legacy/new playback selection: `media-server/src/sources/dvr_playback.rs`.
- Mode responses now use absolute timestamps; subsequent seeks emit a new mode event.
- Tests pass for both codecs, file/memory handoff, recent growth, byte-budget eviction,
  restart, and existing single-file playback. Native Linux camera playback verified.

Current defaults: ten-second keyframe-aligned files, thirty-second/64 MiB recent
buffer, 64 MiB per-segment playback read limit. No automatic retention or unfinished
MP4 recovery. Migration remains uncommitted for user review.
