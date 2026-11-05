use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};

use crate::app::VideoSourceId;

static PREALARM_BASE_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

static DVR_BASE_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn configure_base_dir(path: PathBuf) -> Result<()> {
    anyhow::ensure!(
        !path.as_os_str().is_empty(),
        "DVR directory must not be empty"
    );
    DVR_BASE_DIR
        .set(path)
        .map_err(|_| anyhow::anyhow!("DVR directory was already initialized"))
}
pub fn base_dir() -> &'static std::path::Path {
    DVR_BASE_DIR.get_or_init(|| PathBuf::from("dvr"))
}

pub fn configure_prealarm_dir(path: PathBuf) -> Result<()> {
    anyhow::ensure!(
        !path.as_os_str().is_empty(),
        "Pre-alarm directory must not be empty"
    );
    PREALARM_BASE_DIR
        .set(path)
        .map_err(|_| anyhow::anyhow!("Pre-alarm directory was already initialized"))
}

pub fn prealarm_dir() -> &'static Path {
    PREALARM_BASE_DIR.get_or_init(|| PathBuf::from("prealarm"))
}

pub fn root_for_source(source: &VideoSourceId) -> &'static Path {
    if source.starts_with("prealarm:") {
        prealarm_dir()
    } else {
        base_dir()
    }
}

pub fn get_stream_dvr_dir_in(root: &Path, stream_id: &VideoSourceId) -> PathBuf {
    root.join(derive_safe_fs_name(stream_id))
}

/// Derives a filesystem-safe identifier from source_id using SHA256 hash.
/// This allows any characters in source_id while preventing path traversal attacks.
/// Returns lowercase hex string (64 chars) which is safe on all filesystems.
fn derive_safe_fs_name(source_id: &VideoSourceId) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(source_id.as_bytes());
    hex::encode(hash)
}

pub fn get_stream_dvr_dir(stream_id: &VideoSourceId) -> PathBuf {
    get_stream_dvr_dir_in(root_for_source(stream_id), stream_id)
}

pub fn ensure_stream_dvr_dir(stream_id: &VideoSourceId) -> Result<PathBuf> {
    let dir = get_stream_dvr_dir(stream_id);
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_names_are_hashed_and_roots_stay_separate() {
        let source = "prealarm:../../incident/source".to_owned();
        let live = get_stream_dvr_dir_in(Path::new("live"), &source);
        let archive = get_stream_dvr_dir_in(Path::new("archive"), &source);
        assert_eq!(live.parent(), Some(Path::new("live")));
        assert_eq!(archive.parent(), Some(Path::new("archive")));
        assert_eq!(live.file_name(), archive.file_name());
        assert_eq!(archive.file_name().unwrap().len(), 64);
        assert_ne!(
            get_stream_dvr_dir(&source),
            get_stream_dvr_dir(&"live-camera".into())
        );
        assert_eq!(root_for_source(&source), prealarm_dir());
        assert_eq!(root_for_source(&"camera".into()), base_dir());
    }
}
