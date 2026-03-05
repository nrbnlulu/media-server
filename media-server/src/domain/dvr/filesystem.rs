use anyhow::Result;
use std::fs;
use std::path::PathBuf;

use media_server_api_models::UnixTimestamp;

use crate::app::VideoSourceId;

const DVR_BASE_DIR: &str = "dvr";

/// Derives a filesystem-safe identifier from source_id using SHA256 hash.
/// This allows any characters in source_id while preventing path traversal attacks.
/// Returns lowercase hex string (64 chars) which is safe on all filesystems.
fn derive_safe_fs_name(source_id: &VideoSourceId) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(source_id.as_bytes());
    hex::encode(hash)
}

#[derive(Debug, Clone)]
pub struct RecordingMetadata {
    pub path: PathBuf,
    pub start_time: UnixTimestamp,
    pub end_time: Option<u64>,
}

impl RecordingMetadata {
    pub fn contains(&self, epoch: u64) -> bool {
        self.start_time <= epoch && self.end_time.is_none_or(|end| epoch <= end)
    }
}

pub fn get_stream_dvr_dir(stream_id: &VideoSourceId) -> PathBuf {
    let safe_name = derive_safe_fs_name(stream_id);
    PathBuf::from(DVR_BASE_DIR).join(safe_name)
}

pub fn ensure_stream_dvr_dir(stream_id: &VideoSourceId) -> Result<PathBuf> {
    let dir = get_stream_dvr_dir(stream_id);
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}

pub fn generate_active_recording_filename(start_time: UnixTimestamp) -> String {
    format!("{}_latest.mp4", start_time)
}

pub fn generate_finished_recording_filename(
    start_time: UnixTimestamp,
    end_time: UnixTimestamp,
) -> String {
    format!("{}_{}.mp4", start_time, end_time)
}

pub fn list_recordings_for_stream_id(stream_id: &VideoSourceId) -> Result<Vec<RecordingMetadata>> {
    let dir = get_stream_dvr_dir(stream_id);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut recordings = Vec::new();

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();

        if !path.is_file() {
            continue;
        }

        if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
            if !filename.ends_with(".mp4") {
                continue;
            }

            // Parse <start>_latest.mp4 or <start>_<end>.mp4
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let parts: Vec<&str> = stem.split('_').collect();

            if parts.len() == 2
                && let Ok(start_time) = parts[0].parse::<u64>()
            {
                let end_time = if parts[1] == "latest" {
                    None
                } else {
                    parts[1].parse::<u64>().ok()
                };
                // FIXME: in cases where the video stream was off when trying to record
                // gstreamer would just writeout a no-data fmp4 file that contains only metadata
                // check that file size is greater than 663 (default metadata)
                let file_size = fs::metadata(&path)?.len();
                if file_size > 663 {
                    recordings.push(RecordingMetadata {
                        path,
                        start_time,
                        end_time,
                    });
                }
            }
        }
    }

    // Sort by start time
    recordings.sort_by_key(|r| r.start_time);
    Ok(recordings)
}

pub type FindNextRecRes = Option<(RecordingMetadata, Option<chrono::Duration>)>;

pub fn find_next_recording(stream_id: &VideoSourceId, timestamp: u64) -> FindNextRecRes {
    let mut recordings = list_recordings_for_stream_id(stream_id).ok()?;
    recordings.sort_by_key(|r| r.start_time);

    // First check if there's a recording that contains the timestamp
    for recording in &recordings {
        if recording.contains(timestamp) {
            return Some((recording.clone(), None));
        }
    }

    // If not found, find the first recording that starts AFTER the timestamp
    for recording in recordings {
        if recording.start_time > timestamp {
            let duration =
                chrono::TimeDelta::milliseconds((recording.start_time - timestamp) as i64);
            return Some((recording, Some(duration)));
        }
    }
    None
}

/// finds the recording that contains the timestamp if exists
pub fn find_recording_for_timestamp(
    stream_id: &VideoSourceId,
    timestamp: u64,
) -> Option<RecordingMetadata> {
    let recordings = list_recordings_for_stream_id(stream_id).ok()?;
    for recording in recordings {
        if timestamp >= recording.start_time {
            match recording.end_time {
                Some(end) => {
                    if timestamp <= end {
                        return Some(recording);
                    }
                }
                None => {
                    // This is the active recording (latest)
                    // We assume it contains the timestamp if it started before it.
                    return Some(recording);
                }
            }
        }
    }
    None
}
