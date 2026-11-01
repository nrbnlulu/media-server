# Camera compatibility implementation

Implemented and validated on November 1. Changes remain uncommitted for review.

- Preserve source PTS/DTS using the FFmpeg source timebase. DTS orders/paces
  encoded frames; PTS drives RTP timestamps and reported recording time.
- Pull disk frames through a persistent demuxer with bounded batches. Whole-file
  collection is diagnostic/test-only. A single access unit still has a 64 MiB cap.
- Write fragmented MP4 directly through mp4mux, with approximately one-second
  internal fragments and unbuffered filesink writes. splitmuxsink was removed
  because its GOP lookahead delayed all output for long GOPs.
- Rotate recording pipelines at independent H.264 IDR / H.265 IDR/BLA points,
  after the nominal ten-second file duration. Finalize and catalog the previous
  file. Preserve the session timeline and source timestamp mapping across rotation.
- Include active file metadata in playback discovery. Expose only original bytes
  through the last complete mdat box, using a finite appsrc snapshot. No synthetic
  MP4 index/footer is added. On exhaustion, refresh only when readable data grows.
- Treat recent-buffer duration as a soft target. Under the hard byte budget,
  retain dependency overlap until corresponding disk segments are finalized.
  If the byte budget forces eviction, active-file playback provides a fallback.
- Identify independent decoding anchors by codec NAL type. Search earlier files
  when necessary. Locate the anchor using a bounded scan, reopen there and stream
  frames in batches rather than retaining an entire preroll GOP. Report the actual
  earlier start timestamp to clients on initial and subsequent seeks.

Validation: 41 normal tests passed, two optional probes ignored; all ten generated
compatibility cases pass; Clippy with warnings denied, formatting and whitespace
checks pass. New tests verify that incomplete mdat data is excluded and that active
playback survives cache eviction and follows appended complete fragments without
missing or repeating frames.

Practical limits: seeking can start substantially earlier than requested. Open-GOP
sources with infrequent IDR/BLA points produce longer files and longer preroll.
Growing-file snapshots currently reopen and scan on exhaustion; repeated near-live
reads of a very long file can incur substantial I/O. Exact-target display requires
client-side hidden preroll. Frames not yet in a complete disk fragment remain
available only through RAM. No recording-crash recovery, retention policy, deployment
or physical-camera configuration changes are included.
