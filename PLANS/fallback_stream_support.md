# Fallback Stream Support Plan

## Objective
Add support for multiple RTSP URLs for a single stream to allow fallback capabilities. The system should prioritize the first URL, but if it fails, try subsequent URLs. If a URL works (streams successfully), it should become the sticky "active" URL for retries until it fails immediately.

## Changes

### 1. Update API Models (`crates/media-server-api-models`)
*   **File:** `crates/media-server-api-models/src/lib.rs`
*   **Struct:** `CreateStreamRequest`
    *   Replace `rtsp_url: String` with `rtsp_urls: Vec<String>`.
*   **Struct:** `CreateStreamResponse` & `StreamResponse`
    *   Replace `rtsp_url: String` with `rtsp_urls: Vec<String>`.

### 2. Update Domain Models (`media-server/src/domain`)
*   **File:** `media-server/src/domain/mod.rs`
*   **Struct:** `StreamConfig`
    *   Replace `rtsp_url: url::Url` with `rtsp_urls: Vec<url::Url>`.
*   **Struct:** `StreamInfo`
    *   Replace `rtsp_url: String` with `rtsp_urls: Vec<String>`.

### 3. Update VideoSource Trait (`media-server/src/common`)
*   **File:** `media-server/src/common/traits.rs`
*   **Trait:** `VideoSource`
    *   Change `fn url(&self) -> &url::Url` to `fn urls(&self) -> &[url::Url]`.

### 4. Update RtspClient (`media-server/src/sources/rtsp`)
*   **File:** `media-server/src/sources/rtsp/mod.rs`
*   **Struct:** `RtspClient`
    *   Update to use `config.rtsp_urls`.
*   **Method:** `execute`
    *   Implement the loop with index tracking.
    *   `current_url_index`: usize, starts at 0.
    *   Pass `current_url_index` to the inner loop/pipeline.
    *   Logic:
        *   Attempt connection to `rtsp_urls[current_url_index]`.
        *   If connection "succeeds" (runs for > X seconds or processes > Y packets):
            *   On eventual disconnect, retry same `current_url_index` (sticky).
        *   If connection "fails immediately" (error connecting or immediate EOF):
            *   Increment `current_url_index` (modulo count).
            *   Retry immediately (or with short delay).
*   **Method:** `validate_rtsp_url`
    *   Update to `validate_rtsp_urls` and check all of them.
*   **Method:** `build_rtsp_url`
    *   Accept an index or URL reference.

### 5. Update API Handlers (`media-server/src/api`)
*   **File:** `media-server/src/api/handlers.rs`
*   **Function:** `create_stream`
    *   Parse `req.rtsp_urls`.
    *   Update `StreamConfig` construction.

### 6. Update DVR/Filesystem (If impacted)
*   Check if `filesystem.rs` or `recorder.rs` relies on `config.rtsp_url` for anything other than logging/metadata. (Likely just metadata).

## Verification
*   Compile and check for breaking changes.
*   Test with:
    1.  List of 1 valid URL.
    2.  List of [Invalid, Valid]. Should switch to Valid.
    3.  List of [Valid, Invalid]. Should stick to Valid.
