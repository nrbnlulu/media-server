use anyhow::Result;
use std::fs;
use std::path::PathBuf;

use crate::app::VideoSourceId;

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

/// Derives a filesystem-safe identifier from source_id using SHA256 hash.
/// This allows any characters in source_id while preventing path traversal attacks.
/// Returns lowercase hex string (64 chars) which is safe on all filesystems.
fn derive_safe_fs_name(source_id: &VideoSourceId) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(source_id.as_bytes());
    hex::encode(hash)
}

pub fn get_stream_dvr_dir(stream_id: &VideoSourceId) -> PathBuf {
    let safe_name = derive_safe_fs_name(stream_id);
    base_dir().join(safe_name)
}

pub fn ensure_stream_dvr_dir(stream_id: &VideoSourceId) -> Result<PathBuf> {
    let dir = get_stream_dvr_dir(stream_id);
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}
