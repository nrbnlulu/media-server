# Milesight pre-alarm HTTP import stage

Implement HTTP footage ingestion for the backend's existing Milesight camera/NVR
source-preparation client. Keep media bytes in this service, retain live DVR
behavior, and leave backend B1 and automatic incident jobs deferred.

Implemented:

- Separate `prealarm/` filesystem root, startup recovery and independent retention.
- Optional token-file-protected POST/GET/DELETE import API, shared progress
  WebSocket, and batch clip-existence endpoint.
- Standard HTTP fetching plus an explicit Milesight camera response profile.
  The real firmware nests a malformed second status line in the outer headers;
  merely testing the inner response alone failed to reproduce its parser error.
- Bounded downloads, cancellation, device/global scheduling, retries that replace
  only unfinished attempts, and completed-media reuse across process restart.
- Video-only H.264/H.265 normalization through the DVR writer, codec/IDR checks,
  timestamp repair, and a complete DVR-reader frame-count check before publication.
- Private staging sessions, catalog-aware atomic promotion, crash-staging cleanup,
  and a reserved namespace preventing accidental public live registration.

The frame-count check found a real H.265 B-frame problem: default fragmented MP4
could declare a decode-duration bound that made qtdemux drop presentation frames
at a file boundary. Finalized imports use `first-moov-then-finalise`; live DVR
keeps its original layout because it must remain readable while growing.

Validation includes local HTTP fixtures, the exact double-status response,
authentication, malformed/empty responses, limits, cancellation and replacement,
restart reconciliation, H.264/H.265/B-frame and raw-stream normalization, and
existing-player seeks and finite `ended` behavior. Live camera and NVR exports
also completed through the authenticated HTTP/WebSocket API. Temporary diagnostic
media and credentials are not repository fixtures.

Playback-only signaling is implemented; see `playback-only-sessions.md` for the
session design and validation. Backend B1/B2/B3 and application clip UI are also
implemented. Next: confirm visual playback in the running Flutter UI, then backend
retention-row synchronization. Generic ONVIF replay remains deferred; the current
scope is Milesight.

Authentication (#531), including playback authorization and player token support,
is the final implementation stage after playback, UI and backend integration. It
does not block those stages. Keep the existing import-token protection in place;
it protects import endpoints, not the entire existing API.
