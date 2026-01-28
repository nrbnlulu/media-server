use anyhow::{bail, Result};
use ffmpeg;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::utils::UnixTimestamp;

const DVR_BASE_DIR: &str = "dvr";

#[derive(Debug, Clone)]
pub struct RecordingMetadata {
    pub path: PathBuf,
    pub start_time: UnixTimestamp,
    pub end_time: Option<u64>,
}

impl RecordingMetadata {
    pub fn contains(&self, epoch: u64) -> bool {
        self.start_time <= epoch && self.end_time.map_or(true, |end| epoch <= end)
    }
}
pub fn get_stream_dvr_dir(stream_id: u64) -> PathBuf {
    PathBuf::from(DVR_BASE_DIR).join(stream_id.to_string())
}

pub fn ensure_stream_dvr_dir(stream_id: u64) -> Result<PathBuf> {
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

pub fn list_recordings_for_stream_id(stream_id: u64) -> Result<Vec<RecordingMetadata>> {
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

            if parts.len() == 2 {
                if let Ok(start_time) = parts[0].parse::<u64>() {
                    let mut end_time = if parts[1] == "latest" {
                        None
                    } else {
                        parts[1].parse::<u64>().ok()
                    };
                    // FIXME: in cases where the video stream was off when trying to record
                    // gstreamer would just writeout a no-data fmp4 file that contains only metadata
                    // check that file size is greater than 663 (default metadata)
                    let file_size = fs::metadata(&path)?.len();
                    if file_size > 663 {
                        if end_time.is_none() {
                            if let Some(duration_ms) = probe_mp4_duration_ms(&path) {
                                end_time = Some(start_time.saturating_add(duration_ms));
                            }
                        }
                        recordings.push(RecordingMetadata {
                            path,
                            start_time,
                            end_time,
                        });
                    }
                }
            }
        }
    }

    // Sort by start time
    recordings.sort_by_key(|r| r.start_time);
    Ok(recordings)
}

pub type FindNextRecRes = Option<(RecordingMetadata, Option<chrono::Duration>)>;

pub fn find_next_recording(stream_id: u64, timestamp: u64) -> FindNextRecRes {
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
pub fn find_recording_for_timestamp(stream_id: u64, timestamp: u64) -> Option<RecordingMetadata> {
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

fn probe_mp4_duration_ms(path: &PathBuf) -> Option<u64> {
    let ictx = ffmpeg::format::input(path.to_str()?).ok()?;
    let video_stream = ictx.streams().best(ffmpeg::media::Type::Video)?;
    let time_base = video_stream.time_base();
    let duration_ts = video_stream.duration();
    if duration_ts <= 0 || time_base.denominator() == 0 {
        return None;
    }
    Some(
        ((duration_ts as u128) * 1000 * time_base.numerator() as u128
            / time_base.denominator() as u128) as u64,
    )
}
