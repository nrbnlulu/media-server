//! Recovery of journaled segments and opt-in age-based retention.
use super::{
    reader::{SegmentReader, readable_len},
    segments::{self, Segment},
};
use anyhow::{Result, ensure};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::OnceLock,
};

fn leases() -> &'static Mutex<HashMap<PathBuf, usize>> {
    static LEASES: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    LEASES.get_or_init(Default::default)
}
pub(crate) struct SegmentLease(PathBuf);
impl SegmentLease {
    pub fn acquire(path: &Path) -> Self {
        *leases().lock().entry(path.to_owned()).or_default() += 1;
        Self(path.to_owned())
    }
}
impl Drop for SegmentLease {
    fn drop(&mut self) {
        let mut leases = leases().lock();
        if let Some(count) = leases.get_mut(&self.0) {
            *count -= 1;
            if *count == 0 {
                leases.remove(&self.0);
            }
        }
    }
}

fn directories(root: &Path) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    if !root.exists() {
        return Ok(result);
    }
    for stream in std::fs::read_dir(root)? {
        let stream = stream?;
        if !stream.file_type()?.is_dir() {
            continue;
        }
        for session in std::fs::read_dir(stream.path())? {
            let session = session?;
            if session.file_type()?.is_dir()
                && session
                    .file_name()
                    .to_string_lossy()
                    .starts_with("segments-")
            {
                result.push(session.path());
            }
        }
    }
    Ok(result)
}

/// Runs before any source/recorder is started. Failed recovery preserves the
/// original file under a quarantine suffix rather than advertising broken media.
pub fn recover(root: &Path) -> Result<usize> {
    let mut count = 0;
    for directory in directories(root)? {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let pending = entry.path();
            if !entry.file_type()?.is_file() || pending.extension().is_none_or(|s| s != "pending") {
                continue;
            }
            let path = pending.with_extension("mp4");
            if path.with_extension("json").is_file() {
                if let Err(error) = std::fs::remove_file(&pending) {
                    log::warn!(
                        "Could not remove completed DVR journal {:?}: {error}",
                        pending
                    );
                }
                continue;
            }
            let recovery = (|| -> Result<()> {
                let mut segment: Segment = serde_json::from_slice(&std::fs::read(&pending)?)?;
                segment.path = path.clone();
                segment.validate()?;
                ensure!(
                    std::fs::symlink_metadata(&path)?.file_type().is_file(),
                    "Recovery requires a regular recording file"
                );
                let length = readable_len(&path)?;
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)?
                    .set_len(length)?;
                let mut reader = SegmentReader::open(segment.clone())?;
                let first = reader
                    .next()?
                    .ok_or_else(|| anyhow::anyhow!("No recovered video"))?;
                ensure!(first.key, "Recovered segment has no independent start");
                segment.end_ns = first.pts_ns.saturating_sub(segment.origin_ms * 1_000_000);
                while let Some(frame) = reader.next()? {
                    segment.end_ns = segment
                        .end_ns
                        .max(frame.pts_ns.saturating_sub(segment.origin_ms * 1_000_000));
                }
                drop(reader);
                segment.publish()?;
                if pending.exists() {
                    std::fs::remove_file(&pending)?;
                }
                Ok(())
            })();
            match recovery {
                Ok(()) => {
                    count += 1;
                    log::info!("Recovered DVR recording {:?}", path);
                }
                Err(error) if path.with_extension("json").is_file() => {
                    count += 1;
                    log::warn!(
                        "Recovered DVR media {:?}, but journal cleanup failed: {error:#}",
                        path
                    );
                }
                Err(error) => {
                    log::error!(
                        "DVR recovery failed for {:?}; retaining quarantined files: {error:#}",
                        path
                    );
                    if path.exists()
                        && let Err(error) =
                            std::fs::rename(&path, path.with_extension("mp4.unrecoverable"))
                    {
                        log::error!(
                            "Could not quarantine {:?}; retaining original file and journal: {error}",
                            path
                        );
                        continue;
                    }
                    if let Err(error) =
                        std::fs::rename(&pending, pending.with_extension("pending.unrecoverable"))
                    {
                        log::error!(
                            "Could not quarantine journal {:?}; retaining it: {error}",
                            pending
                        );
                    }
                }
            }
        }
    }
    // Warm each stream after recovery, so playback does not pay the initial scan.
    let streams: std::collections::BTreeSet<_> = directories(root)?
        .into_iter()
        .filter_map(|directory| directory.parent().map(Path::to_owned))
        .collect();
    for stream in streams {
        if let Err(error) = segments::list_stream(&stream) {
            // Preserve startup behavior: a bad catalog fails playback for its
            // stream, but does not prevent unrelated cameras from starting.
            log::warn!("Could not preload DVR catalog {stream:?}: {error:#}");
        }
    }
    Ok(count)
}

/// Delete only expired cataloged segments. Pending, quarantined, active and open
/// files are excluded. The catalog is withdrawn before removing the media file.
pub fn retain(root: &Path, now_ms: u64, seconds: u64) -> Result<usize> {
    ensure!(seconds > 0, "Retention duration must be positive");
    let cutoff = now_ms.saturating_sub(seconds.saturating_mul(1000));
    let active = segments::active_paths();
    let mut removed = 0;
    for directory in directories(root)? {
        for segment in segments::list_in(&directory)? {
            if segment.end_ms() >= cutoff
                || active.contains(&segment.path)
                || segment.path.with_extension("pending").exists()
            {
                continue;
            }
            let leases = leases().lock();
            if leases.contains_key(&segment.path) {
                continue;
            }
            segments::withdraw(&segment)?;
            if let Err(error) = std::fs::remove_file(&segment.path) {
                // Keep it discoverable for a later cleanup attempt if deletion
                // fails after withdrawing its catalog entry.
                if let Err(restore) = segment.publish() {
                    log::error!(
                        "Failed to restore retained DVR catalog {:?}: {restore:#}",
                        segment.path
                    );
                }
                return Err(error.into());
            }
            drop(leases);
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("dvr-maintenance-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn directory(&self) -> PathBuf {
            let directory = self.0.join("stream").join("segments-test");
            std::fs::create_dir_all(&directory).unwrap();
            directory
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn segment(directory: &Path, index: u64, end_ms: u64) -> Segment {
        let path = directory.join(format!("{index:010}.mp4"));
        std::fs::write(&path, b"recording").unwrap();
        Segment {
            path,
            origin_ms: 1000,
            first_ns: 0,
            end_ns: end_ms * 1_000_000,
            codec: "h264".into(),
        }
    }
    #[test]
    fn recovers_complete_fragments_and_quarantines_unreadable_tail() {
        gstreamer::init().unwrap();
        for (codec, bytes) in [
            (
                "h264",
                include_bytes!("../../../tests/fixtures/dvr-fragmented.mp4").as_slice(),
            ),
            (
                "h265",
                include_bytes!("../../../tests/fixtures/dvr-fragmented-h265.mp4").as_slice(),
            ),
        ] {
            let root = Root::new();
            let directory = root.directory();
            let mut recorded = segment(&directory, 0, 0);
            recorded.codec = codec.into();
            recorded.journal().unwrap();
            std::fs::write(&recorded.path, &bytes[..bytes.len() - 7]).unwrap();
            let broken = segment(&directory, 1, 0);
            broken.journal().unwrap();
            std::fs::write(&broken.path, b"bad").unwrap();
            assert_eq!(recover(&root.0).unwrap(), 1);
            assert!(recorded.path.with_extension("json").exists());
            assert!(!recorded.path.with_extension("pending").exists());
            assert!(recorded.path.metadata().unwrap().len() < (bytes.len() - 7) as u64);
            let catalog = segments::list_in(&directory).unwrap();
            assert_eq!(catalog.len(), 1);
            assert!(catalog[0].end_ms() > 11000);
            assert!(broken.path.with_extension("mp4.unrecoverable").exists());
            assert!(!broken.path.with_extension("json").exists());
            assert_eq!(recover(&root.0).unwrap(), 0);
        }
    }
    #[test]
    fn retention_protects_open_active_pending_and_recent_recordings() {
        let root = Root::new();
        let directory = root.directory();
        let expired = segment(&directory, 0, 1000);
        expired.publish().unwrap();
        let open = segment(&directory, 1, 1000);
        open.publish().unwrap();
        let lease = SegmentLease::acquire(&open.path);
        let pending = segment(&directory, 2, 1000);
        pending.publish().unwrap();
        pending.journal().unwrap();
        let recent = segment(&directory, 3, 15000);
        recent.publish().unwrap();
        let active = segment(&directory, 4, 1000);
        active.publish().unwrap();
        let source = format!("retention-{}", uuid::Uuid::new_v4());
        let session = segments::Session::in_directory(
            &source,
            1000,
            crate::common::VideoCodec::H264,
            directory.clone(),
        )
        .unwrap();
        session.active_files.lock().push(active.clone());
        assert_eq!(retain(&root.0, 20000, 10).unwrap(), 1);
        assert!(!expired.path.exists());
        for segment in [&open, &pending, &recent, &active] {
            assert!(segment.path.exists());
        }
        drop(lease);
        assert_eq!(retain(&root.0, 20000, 10).unwrap(), 1);
        assert!(!open.path.exists());
        assert!(retain(&root.0, 20000, 0).is_err());
    }
    #[test]
    fn recovery_finishes_a_published_segment_without_truncating_it() {
        let root = Root::new();
        let directory = root.directory();
        let recorded = segment(&directory, 0, 1000);
        recorded.publish().unwrap();
        recorded.journal().unwrap();
        let before = std::fs::read(&recorded.path).unwrap();
        assert_eq!(recover(&root.0).unwrap(), 0);
        assert_eq!(std::fs::read(&recorded.path).unwrap(), before);
        assert!(!recorded.path.with_extension("pending").exists());
    }
}
