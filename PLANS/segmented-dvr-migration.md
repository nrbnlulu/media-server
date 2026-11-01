# Segmented DVR migration

Current status: segmented recording and playback only. The legacy single-file
player, backend selector, virtual index, old-file catalog and filename helpers
have been removed. Existing recording data was not deleted.

Recording: `media-server/src/domain/dvr/recorder.rs`.
Catalog and recent buffer: `media-server/src/domain/dvr/segments.rs`.
Playback: `media-server/src/sources/segmented_dvr.rs`.

See `PLANS/camera-compatibility-fixes.md` and `REVIEW_FINDINGS.md` for the current
architecture, compatibility checks and limitations.
