# Safe DVR/live switching and startup heartbeat

First: use generation-scoped inputs and serialize complete access-unit delivery.
Reject stale cloned live inputs, gate live on independent frames, include codec
parameter sets, and drop obsolete queued WSC media at a switch.

Second: independently receive WebSocket heartbeat traffic during SDP/UDP startup,
use monotonic timeout, stop on EOF and cancel connection tasks on exit. Verify
stale inputs, fragmented frame switching, queue generations, startup pongs, timeout
and EOF with focused regression tests, then run workspace checks.


Completed: generation-scoped inputs; complete-frame serialization; independent live
frame gating with codec headers; old queued WSC generation rejection; wire-side
sequence/timestamp stitching; mode-before-live subscription; independently running
WebSocket reader/writer; monotonic receive inactivity timeout; startup pongs; EOF
cleanup; write deadlines; and initial-SDP deduplication.

Validation: 41 tests passed, one optional probe ignored; Clippy, formatting and
whitespace checks passed. No deployment or physical-camera/network validation.
See REVIEW_FINDINGS.md for the detailed current status and limits.
