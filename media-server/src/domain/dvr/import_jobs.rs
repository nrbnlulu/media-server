//! In-memory import orchestration. Completed clips are immutable and survive restarts.
use super::{
    filesystem,
    http_import::{DownloadError, HttpCredentials, HttpImporter, HttpResponseProfile},
    import_normalize::{self, ImportResult},
};
use anyhow::{Result, ensure};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex as AsyncMutex, Semaphore, broadcast};
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRequest {
    pub import_id: String,
    pub media_source_id: String,
    pub kind: ImportKind,
    pub url: Url,
    pub creds: Option<HttpCredentials>,
    pub from_ms: i64,
    pub to_ms: i64,
    pub clock_offset_ms: i64,
    #[serde(default)]
    pub http_response_profile: HttpResponseProfile,
}
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportKind {
    Http,
}
impl ImportRequest {
    pub fn validate(&self) -> Result<u64> {
        ensure!(
            !self.import_id.is_empty() && self.import_id != "ws" && self.import_id.len() <= 256,
            "Invalid import_id"
        );
        ensure!(
            self.media_source_id.starts_with("prealarm:")
                && self.media_source_id.len() > 9
                && self.media_source_id.len() <= 512,
            "Expected a prealarm: media_source_id"
        );
        ensure!(
            (1..=120_000).contains(&(i128::from(self.to_ms) - i128::from(self.from_ms))),
            "Import window must be positive and at most 120 seconds"
        );
        ensure!(
            matches!(self.url.scheme(), "http" | "https")
                && self.url.host_str().is_some()
                && self.url.username().is_empty()
                && self.url.password().is_none(),
            "Expected HTTP URL with separate credentials"
        );
        ensure!(
            self.http_response_profile != HttpResponseProfile::MilesightCamera
                || self.url.path() == "/dataloader.cgi",
            "Camera HTTP compatibility is restricted to /dataloader.cgi"
        );
        let origin = i128::from(self.from_ms) - i128::from(self.clock_offset_ms);
        ensure!(
            (0..=i128::from(u64::MAX / 1_000_000)).contains(&origin),
            "Corrected recording origin is out of range"
        );
        Ok(origin as u64)
    }
}
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImportState {
    Pending,
    Downloading,
    Completed,
    Failed,
    Cancelled,
}
#[derive(Clone, Debug, Serialize)]
pub struct ImportStatus {
    pub import_id: String,
    pub state: ImportState,
    pub progress: Option<f64>,
    pub error: Option<String>,
    pub result: Option<ImportResult>,
}
struct Entry {
    media_source_id: String,
    status: Mutex<ImportStatus>,
    progress_at: Mutex<Instant>,
    cancel: CancellationToken,
    task: AsyncMutex<Option<tokio::task::JoinHandle<()>>>,
}
pub struct ImportManager {
    root: PathBuf,
    http: HttpImporter,
    jobs: DashMap<String, Arc<Entry>>,
    ids: DashMap<String, Arc<AsyncMutex<()>>>,
    sources: DashMap<String, Arc<AsyncMutex<()>>>,
    devices: DashMap<String, Arc<AsyncMutex<()>>>,
    slots: Semaphore,
    events: broadcast::Sender<ImportStatus>,
}
impl ImportManager {
    pub fn new(root: PathBuf) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&root)?;
        cleanup_staging(&root)?;
        Ok(Arc::new(Self {
            root,
            http: HttpImporter::new(512 * 1024 * 1024, Duration::from_secs(120))?,
            jobs: DashMap::new(),
            ids: DashMap::new(),
            sources: DashMap::new(),
            devices: DashMap::new(),
            slots: Semaphore::new(4),
            events: broadcast::channel(256).0,
        }))
    }
    pub fn subscribe(&self) -> broadcast::Receiver<ImportStatus> {
        self.events.subscribe()
    }
    pub fn get(&self, id: &str) -> Option<ImportStatus> {
        self.jobs.get(id).map(|e| e.status.lock().clone())
    }
    fn emit(
        &self,
        entry: &Entry,
        state: ImportState,
        progress: Option<f64>,
        error: Option<String>,
        result: Option<ImportResult>,
    ) {
        let mut status = entry.status.lock();
        status.state = state;
        status.progress = progress;
        status.error = error;
        status.result = result;
        let _ = self.events.send(status.clone());
    }
    pub async fn start(self: &Arc<Self>, request: ImportRequest) -> Result<ImportStatus> {
        request.validate()?;
        let lock = self
            .ids
            .entry(request.import_id.clone())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        let previous = self.jobs.get(&request.import_id).map(|e| e.value().clone());
        if let Some(previous) = previous {
            ensure!(
                previous.media_source_id == request.media_source_id,
                "An import_id cannot change its media_source_id"
            );
            // Reposting replaces an unfinished attempt, never a valid completed clip.
            if previous.status.lock().state == ImportState::Completed
                && completed_in(&filesystem::get_stream_dvr_dir_in(
                    &self.root,
                    &request.media_source_id,
                ))?
                .is_some()
            {
                return Ok(previous.status.lock().clone());
            }
            previous.cancel.cancel();
            if let Some(task) = previous.task.lock().await.take() {
                let _ = task.await;
            }
        }
        let entry = Arc::new(Entry {
            media_source_id: request.media_source_id.clone(),
            status: Mutex::new(ImportStatus {
                import_id: request.import_id.clone(),
                state: ImportState::Pending,
                progress: None,
                error: None,
                result: None,
            }),
            progress_at: Mutex::new(Instant::now() - Duration::from_secs(1)),
            cancel: CancellationToken::new(),
            task: AsyncMutex::new(None),
        });
        self.jobs.insert(request.import_id.clone(), entry.clone());
        let initial = entry.status.lock().clone();
        let _ = self.events.send(initial.clone());
        let manager = self.clone();
        let worker = entry.clone();
        *entry.task.lock().await = Some(tokio::spawn(async move {
            let operation = manager.run(&request, &worker);
            tokio::pin!(operation);
            let (outcome, timed_out) = tokio::select! {
                result = &mut operation => (result, false),
                _ = tokio::time::sleep(Duration::from_secs(180)) => {
                    worker.cancel.cancel();
                    let _ = operation.await;
                    (Err("timeout".into()), true)
                }
            };
            match outcome {
                Ok(result) => manager.emit(
                    &worker,
                    ImportState::Completed,
                    Some(1.0),
                    None,
                    Some(result),
                ),
                Err(_) if !timed_out && worker.cancel.is_cancelled() => {
                    manager.emit(&worker, ImportState::Cancelled, None, None, None)
                }
                Err(error) => manager.emit(&worker, ImportState::Failed, None, Some(error), None),
            }
        }));
        Ok(initial)
    }
    pub async fn cancel(&self, id: &str) -> bool {
        let entry = self.jobs.get(id).map(|e| e.value().clone());
        let Some(entry) = entry else { return false };
        entry.cancel.cancel();
        if let Some(task) = entry.task.lock().await.take() {
            let _ = task.await;
        }
        true
    }
    pub async fn shutdown(&self) {
        let entries: Vec<_> = self.jobs.iter().map(|e| e.value().clone()).collect();
        for e in &entries {
            e.cancel.cancel();
        }
        for e in entries {
            if let Some(task) = e.task.lock().await.take() {
                let _ = task.await;
            }
        }
    }
    async fn run(&self, request: &ImportRequest, entry: &Entry) -> Result<ImportResult, String> {
        let source_lock = self
            .sources
            .entry(request.media_source_id.clone())
            .or_default()
            .clone();
        let _source = tokio::select! {
            biased;
            _=entry.cancel.cancelled()=>return Err("cancelled".into()),
            guard=source_lock.lock()=>guard,
        };
        let final_directory =
            filesystem::get_stream_dvr_dir_in(&self.root, &request.media_source_id);
        if let Some(result) = completed_in(&final_directory).map_err(|_| "corrupt".to_string())? {
            return Ok(result);
        }
        let device_key = format!(
            "{}://{}:{}",
            request.url.scheme(),
            request.url.host_str().unwrap_or_default(),
            request.url.port_or_known_default().unwrap_or_default()
        );
        let device_lock = self.devices.entry(device_key).or_default().clone();
        let _device = tokio::select! {
            biased;
            _=entry.cancel.cancelled()=>return Err("cancelled".into()),
            guard=device_lock.lock()=>guard,
        };
        let _slot = tokio::select! {
            biased;
            _=entry.cancel.cancelled()=>return Err("cancelled".into()),
            slot=self.slots.acquire()=>slot.map_err(|_|"cancelled".to_string())?,
        };
        let origin = request
            .validate()
            .map_err(|_| "invalid_request".to_string())?;
        let stage_id = format!("prealarm:staging:{}", uuid::Uuid::new_v4());
        let stage = Staging::new(filesystem::get_stream_dvr_dir_in(&self.root, &stage_id))
            .map_err(|_| "storage_failed".to_string())?;
        let input = stage.0.join("download.tmp");
        self.emit(entry, ImportState::Downloading, None, None, None);
        let download = self
            .http
            .download(
                request.url.clone(),
                request.creds.as_ref(),
                request.http_response_profile,
                &input,
                &entry.cancel,
                |count, total| {
                    let mut at = entry.progress_at.lock();
                    if at.elapsed() >= Duration::from_secs(1) {
                        *at = Instant::now();
                        self.emit(
                            entry,
                            ImportState::Downloading,
                            total.map(|total| {
                                if total == 0 {
                                    0.0
                                } else {
                                    count as f64 / total as f64
                                }
                            }),
                            None,
                            None,
                        );
                    }
                },
            )
            .await;
        download.map_err(|e| {
            log::warn!("Recording import download failed: {e:?}");
            match e {
                DownloadError::NoFootage => "no_footage",
                DownloadError::TooLarge => "too_large",
                DownloadError::Cancelled => "cancelled",
                _ => "download_failed",
            }
            .to_string()
        })?;
        let directory = stage
            .0
            .join(format!("segments-{origin}-{}", uuid::Uuid::new_v4()));
        let cancel = entry.cancel.clone();
        let copied = directory.clone();
        let result = tokio::task::spawn_blocking(move || {
            import_normalize::normalize(&input, &copied, origin, &cancel)
        })
        .await
        .map_err(|_| "normalization_failed".to_string())?
        .map_err(|error| {
            let text = error.to_string();
            if text.starts_with("no_footage:") {
                "no_footage"
            } else if text.starts_with("unsupported_codec:") {
                "unsupported_codec"
            } else {
                "corrupt"
            }
            .to_string()
        })?;
        if entry.cancel.is_cancelled() {
            return Err("cancelled".into());
        }
        // Move only a completely finalized tree into its deterministic source path.
        // Staging's hashed source id remains invisible to lookups for the real source.
        std::fs::remove_file(stage.0.join("download.tmp"))
            .map_err(|_| "storage_failed".to_string())?;
        std::fs::remove_file(stage.0.join(".import-staging"))
            .map_err(|_| "storage_failed".to_string())?;
        match super::segments::promote_stream(&stage.0, &final_directory) {
            Ok(()) => Ok(result),
            Err(_) => completed_in(&final_directory)
                .map_err(|_| "storage_failed".to_string())?
                .ok_or_else(|| "storage_failed".to_string()),
        }
    }
    pub fn exists(&self, ids: &[String]) -> Vec<String> {
        ids.iter()
            .filter(|id| {
                id.starts_with("prealarm:")
                    && completed_in(&filesystem::get_stream_dvr_dir_in(&self.root, id))
                        .ok()
                        .flatten()
                        .is_some()
            })
            .cloned()
            .collect()
    }
}
pub(crate) fn completed_in(root: &Path) -> Result<Option<ImportResult>> {
    if !root.exists() || root.join(".import-staging").exists() {
        return Ok(None);
    }
    ensure!(
        std::fs::symlink_metadata(root)?.file_type().is_dir(),
        "Import source must be a regular directory"
    );
    import_normalize::result_from_segments(super::segments::list_stream(root)?)
}

struct Staging(PathBuf);
impl Staging {
    fn new(path: PathBuf) -> Result<Self> {
        std::fs::create_dir(&path)?;
        let guard = Self(path);
        std::fs::write(guard.0.join(".import-staging"), b"")?;
        Ok(guard)
    }
}
impl Drop for Staging {
    fn drop(&mut self) {
        super::segments::forget_stream(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Only abandoned, owned import trees are eligible. Active jobs have a two-minute
/// download deadline and write their marker after acquiring their concurrency slot.
pub fn cleanup_staging(root: &Path) -> Result<usize> {
    if !root.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.len() != 64 || !name.bytes().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let marker = entry.path().join(".import-staging");
        let Ok(meta) = std::fs::symlink_metadata(marker) else {
            continue;
        };
        if meta.is_file()
            && meta
                .modified()?
                .elapsed()
                .is_ok_and(|age| age >= Duration::from_secs(3600))
        {
            super::segments::forget_stream(&entry.path());
            std::fs::remove_dir_all(entry.path())?;
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    use tower::ServiceExt;

    pub(crate) async fn serve(
        data: Vec<u8>,
        malformed: bool,
    ) -> (Url, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/dataloader.cgi", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut buf = [0; 1024];
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    return;
                }
                request.extend_from_slice(&buf[..n]);
            }
            let header = if malformed {
                format!(
                    "HTTP/1.0 200 OK\r\nServer: WebServer\r\nConnection: close\r\nHTTP/1.0 200 OK/nServer: lion\nContent-Type: application/octet-stream/nContent-Length: {}\nConnection: close\n\n",
                    data.len()
                )
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    data.len()
                )
            };
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(&data).await;
        });
        (url, task)
    }
    fn root() -> PathBuf {
        std::env::temp_dir().join(format!("ms-import-job-{}", uuid::Uuid::new_v4()))
    }
    pub(super) fn request(url: Url) -> ImportRequest {
        ImportRequest {
            import_id: "job-1".into(),
            media_source_id: "prealarm:incident:source".into(),
            kind: ImportKind::Http,
            url,
            creds: None,
            from_ms: 1_700_000_300_000,
            to_ms: 1_700_000_319_000,
            clock_offset_ms: 300_000,
            http_response_profile: HttpResponseProfile::Standard,
        }
    }
    pub(crate) async fn wait(manager: &ImportManager, id: &str) -> ImportStatus {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let status = manager.get(id).unwrap();
                if matches!(
                    status.state,
                    ImportState::Completed | ImportState::Failed | ImportState::Cancelled
                ) {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap()
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn real_http_imports_publish_both_codecs_and_reconcile_after_restart() {
        for hevc in [false, true] {
            let root = root();
            std::fs::create_dir_all(&root).unwrap();
            let sample = root.join("sample.mp4");
            import_normalize::tests::fixture(&sample, hevc, false);
            let (url, server) = serve(std::fs::read(&sample).unwrap(), hevc).await;
            let manager = ImportManager::new(root.join("archive")).unwrap();
            let mut events = manager.subscribe();
            let mut request = request(url);
            if hevc {
                request.http_response_profile = HttpResponseProfile::MilesightCamera;
            }
            let destination =
                filesystem::get_stream_dvr_dir_in(&manager.root, &request.media_source_id);
            // A UI/reconciler may ask about a clip before its first import.
            assert!(
                super::super::segments::list_stream(&destination)
                    .unwrap()
                    .is_empty()
            );
            manager.start(request.clone()).await.unwrap();
            let status = wait(&manager, &request.import_id).await;
            assert_eq!(status.state, ImportState::Completed, "{:?}", status.error);
            let result = status.result.unwrap();
            assert_eq!(result.actual_start_ms, 1_700_000_000_000);
            assert_eq!(result.codec, if hevc { "h265" } else { "h264" });
            assert_eq!(
                manager.exists(std::slice::from_ref(&request.media_source_id)),
                vec![request.media_source_id.clone()]
            );
            assert!(completed_in(&destination).unwrap().is_some());
            assert!(!destination.join(".import-staging").exists());
            assert!(!destination.join("download.tmp").exists());
            assert!(events.try_recv().is_ok());
            server.await.unwrap();
            // A repeated POST or a new process must use the completed clip without
            // needing the expired vendor URL or opening another playback session.
            assert_eq!(
                manager.start(request.clone()).await.unwrap().state,
                ImportState::Completed
            );
            manager.shutdown().await;
            let restarted = ImportManager::new(manager.root.clone()).unwrap();
            restarted.start(request.clone()).await.unwrap();
            assert_eq!(
                wait(&restarted, &request.import_id).await.state,
                ImportState::Completed
            );
            restarted.shutdown().await;
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    #[tokio::test]
    async fn auth_precedes_parsing_and_ids_cannot_change_source() {
        let root = root();
        let manager = ImportManager::new(root.clone()).unwrap();
        let app = crate::api::imports::router(manager.clone(), "secret".into());
        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/imports")
                    .body(Body::from("invalid json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        let missing = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/imports/absent")
                    .header("authorization", "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let json = serde_json::json!({"import_id":"api-job","media_source_id":"prealarm:api:test","kind":"http","url":"http://127.0.0.1:1/","from_ms":1000,"to_ms":2000,"clock_offset_ms":0});
        let accepted = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/imports")
                    .header("authorization", "Bearer secret")
                    .header("content-type", "application/json")
                    .body(Body::from(json.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::ACCEPTED);
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(accepted.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(value["import_id"], "api-job");
        let mut changed = request("http://127.0.0.1:1/".parse().unwrap());
        changed.import_id = "api-job".into();
        assert!(manager.start(changed).await.is_err());
        manager.shutdown().await;
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn no_footage_and_corrupt_media_do_not_publish_or_leave_staging() {
        import_normalize::tests::init_media();
        for data in [b"<html>no recording</html>".to_vec(), vec![1, 2, 3, 4]] {
            let root = root();
            let manager = ImportManager::new(root.clone()).unwrap();
            let (url, server) = serve(data.clone(), false).await;
            let request = request(url);
            manager.start(request.clone()).await.unwrap();
            let status = wait(&manager, &request.import_id).await;
            assert_eq!(status.state, ImportState::Failed);
            assert_eq!(
                status.error.as_deref(),
                Some(if data[0] == b'<' {
                    "no_footage"
                } else {
                    "corrupt"
                })
            );
            assert!(manager.exists(&[request.media_source_id]).is_empty());
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
            server.await.unwrap();
            manager.shutdown().await;
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn validation_checks_overflows_urls_and_reserved_ids() {
        let good = request("http://127.0.0.1/test".parse().unwrap());
        assert_eq!(good.validate().unwrap(), 1_700_000_000_000);
        for (from, to, offset) in [
            (100, 100, 0),
            (100, 50, 0),
            (0, 120001, 0),
            (i64::MIN, i64::MAX, 0),
            (0, 100, 1),
        ] {
            let mut bad = good.clone();
            bad.from_ms = from;
            bad.to_ms = to;
            bad.clock_offset_ms = offset;
            assert!(bad.validate().is_err());
        }
        let mut bad = good.clone();
        bad.import_id = "ws".into();
        assert!(bad.validate().is_err());
        bad = good.clone();
        bad.media_source_id = "live-camera".into();
        assert!(bad.validate().is_err());
        bad = good.clone();
        bad.url = "http://user:password@127.0.0.1/".parse().unwrap();
        assert!(bad.validate().is_err());
    }
}

#[cfg(test)]
mod playback_tests {
    use super::*;
    use crate::{
        common::{VideoCodec, rtp::RtpPacket, traits::RtpConsumer},
        sources::segmented_dvr::SegmentedPlayer,
    };
    use media_server_api_models::{ClientSessionId, PlaybackEvent};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Capture {
        id: ClientSessionId,
        packets: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl RtpConsumer for Capture {
        fn id(&self) -> &ClientSessionId {
            &self.id
        }
        async fn on_new_packet(&self, _packet: Arc<RtpPacket>) {
            self.packets.fetch_add(1, Ordering::Relaxed);
        }
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn imported_clips_seek_in_the_existing_player_and_end_without_a_live_source() {
        for hevc in [false, true] {
            let root =
                std::env::temp_dir().join(format!("ms-import-playback-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let input = root.join("input.mp4");
            import_normalize::tests::fixture(&input, hevc, false);
            let (url, server) = super::tests::serve(std::fs::read(input).unwrap(), false).await;
            let manager = ImportManager::new(filesystem::prealarm_dir().to_owned()).unwrap();
            let source = format!("prealarm:test:{}", uuid::Uuid::new_v4());
            let request = ImportRequest {
                import_id: source.clone(),
                media_source_id: source.clone(),
                kind: ImportKind::Http,
                url,
                creds: None,
                from_ms: 1_700_000_000_000,
                to_ms: 1_700_000_019_000,
                clock_offset_ms: 0,
                http_response_profile: HttpResponseProfile::Standard,
            };
            manager.start(request.clone()).await.unwrap();
            let status = super::tests::wait(&manager, &request.import_id).await;
            assert_eq!(status.state, ImportState::Completed, "{:?}", status.error);
            let result = status.result.unwrap();
            assert!(super::super::segments::Session::active(&source).is_none());
            let params = SegmentedPlayer::codec_parameters(&source, result.actual_start_ms)
                .await
                .unwrap();
            assert_eq!(
                params.codec(),
                if hevc {
                    VideoCodec::H265
                } else {
                    VideoCodec::H264
                }
            );
            for offset in [0, 1500, 12300] {
                let capture = Arc::new(Capture {
                    id: uuid::Uuid::new_v4(),
                    packets: AtomicUsize::new(0),
                });
                let target = result.actual_start_ms + offset;
                let player = SegmentedPlayer::new(source.clone(), target, capture.clone())
                    .await
                    .unwrap();
                let actual = player.current_timestamp().await;
                assert!(
                    actual <= target && target - actual <= 1100,
                    "target {target}, got {actual}"
                );
                if offset == 12300 {
                    player.play().await.unwrap();
                    tokio::time::timeout(Duration::from_secs(5), async {
                        loop {
                            match player.playback_event() {
                                PlaybackEvent::Ended { .. } => break,
                                PlaybackEvent::Error { message, .. } => panic!("{message}"),
                                _ => tokio::time::sleep(Duration::from_millis(10)).await,
                            }
                        }
                    })
                    .await
                    .unwrap();
                    assert!(capture.packets.load(Ordering::Relaxed) > 0);
                }
                player.terminate().await.unwrap();
            }
            server.await.unwrap();
            manager.shutdown().await;
            let directory = filesystem::get_stream_dvr_dir(&source);
            super::super::segments::forget_stream(&directory);
            std::fs::remove_dir_all(directory).unwrap();
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[tokio::test]
    async fn replacing_a_pending_attempt_cancels_its_download_and_cleans_up() {
        let root = std::env::temp_dir().join(format!("ms-replace-{}", uuid::Uuid::new_v4()));
        let manager = ImportManager::new(root.clone()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url: Url = format!("http://{}/clip", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let (started, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 4096];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\npartial")
                .await
                .unwrap();
            let _ = started.send(());
            // The replacement must close the first response rather than waiting
            // for its HTTP timeout or publishing a partial file.
            loop {
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    _ => {}
                }
            }
        });
        let mut request = super::tests::request(url);
        manager.start(request.clone()).await.unwrap();
        received.await.unwrap();
        let (url, second) = super::tests::serve(b"<html>no footage</html>".to_vec(), false).await;
        request.url = url;
        tokio::time::timeout(Duration::from_secs(3), manager.start(request.clone()))
            .await
            .unwrap()
            .unwrap();
        let status = super::tests::wait(&manager, &request.import_id).await;
        assert_eq!(status.state, ImportState::Failed);
        assert_eq!(status.error.as_deref(), Some("no_footage"));
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        second.await.unwrap();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        assert!(manager.cancel(&request.import_id).await);
        assert!(!manager.cancel("missing").await);
        manager.shutdown().await;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn abandoned_staging_cleanup_preserves_active_and_unmarked_directories() {
        let root = std::env::temp_dir().join(format!("ms-staging-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let old = filesystem::get_stream_dvr_dir_in(&root, &"prealarm:staging:old".into());
        std::fs::create_dir_all(&old).unwrap();
        let file = std::fs::File::create(old.join(".import-staging")).unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(7200)),
        )
        .unwrap();
        std::fs::write(old.join("partial-download"), b"partial").unwrap();
        let active = Staging::new(filesystem::get_stream_dvr_dir_in(
            &root,
            &"prealarm:staging:active".into(),
        ))
        .unwrap();
        let unmarked = filesystem::get_stream_dvr_dir_in(&root, &"prealarm:completed:clip".into());
        std::fs::create_dir_all(&unmarked).unwrap();
        std::fs::write(unmarked.join("footage"), b"keep").unwrap();
        assert_eq!(cleanup_staging(&root).unwrap(), 1);
        assert!(!old.exists());
        assert!(active.0.exists());
        assert!(unmarked.join("footage").exists());
        drop(active);
        std::fs::remove_dir_all(root).unwrap();
    }
}
