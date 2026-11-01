//! Finalized MP4 catalog and bounded, shared recent encoded video.
use crate::{app::VideoSourceId, common::VideoCodec};
use anyhow::{Result, anyhow, ensure};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

pub const SEGMENT_NS: u64 = 10_000_000_000;
const BUFFER_NS: u64 = 30_000_000_000;
const BUFFER_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct Frame {
    pub pts_ns: u64,
    pub key: bool,
    pub data: Arc<Vec<u8>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Segment {
    pub origin_ms: u64,
    pub first_ns: u64,
    pub end_ns: u64,
    pub codec: String,
    #[serde(skip)]
    pub path: PathBuf,
}
impl Segment {
    pub fn codec(&self) -> Result<VideoCodec> {
        VideoCodec::from_encoding_name(&self.codec).ok_or_else(|| anyhow!("Unsupported DVR codec"))
    }
    pub fn start_ms(&self) -> u64 {
        self.origin_ms + self.first_ns / 1_000_000
    }
    pub fn end_ms(&self) -> u64 {
        self.origin_ms + self.end_ns / 1_000_000
    }
    pub fn publish(&self) -> Result<()> {
        let path = self.path.with_extension("json");
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec(self)?)?;
        std::fs::rename(temporary, path)?;
        Ok(())
    }
}

#[derive(Default)]
struct Recent {
    frames: VecDeque<Frame>,
    bytes: usize,
}

pub struct Session {
    pub origin_ms: u64,
    pub codec: VideoCodec,
    pub directory: PathBuf,
    recent: Mutex<Recent>,
    pub closed: AtomicBool,
    pub started: std::time::Instant,
}

fn sessions() -> &'static Mutex<HashMap<VideoSourceId, Weak<Session>>> {
    static SESSIONS: OnceLock<Mutex<HashMap<VideoSourceId, Weak<Session>>>> = OnceLock::new();
    SESSIONS.get_or_init(Default::default)
}

impl Session {
    pub fn new(source: &VideoSourceId, origin_ms: u64, codec: VideoCodec) -> Result<Arc<Self>> {
        let directory = super::filesystem::ensure_stream_dvr_dir(source)?
            .join(format!("segments-{origin_ms}-{}", uuid::Uuid::new_v4()));
        Self::in_directory(source, origin_ms, codec, directory)
    }
    pub fn in_directory(
        source: &VideoSourceId,
        origin_ms: u64,
        codec: VideoCodec,
        directory: PathBuf,
    ) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&directory)?;
        let session = Arc::new(Self {
            origin_ms,
            codec,
            directory,
            recent: Mutex::new(Recent::default()),
            closed: AtomicBool::new(false),
            started: std::time::Instant::now(),
        });
        sessions()
            .lock()
            .insert(source.clone(), Arc::downgrade(&session));
        Ok(session)
    }
    pub fn active(source: &VideoSourceId) -> Option<Arc<Self>> {
        sessions().lock().get(source).and_then(Weak::upgrade)
    }
    pub fn push(&self, frame: Frame) {
        let mut recent = self.recent.lock();
        if recent.frames.is_empty() && !frame.key {
            return;
        }
        recent.bytes += frame.data.len();
        let newest = frame.pts_ns;
        recent.frames.push_back(frame);
        // Evict complete GOPs. Never leave a delta frame as the first frame.
        while recent.bytes > BUFFER_BYTES
            || recent
                .frames
                .front()
                .is_some_and(|f| newest.saturating_sub(f.pts_ns) > BUFFER_NS)
        {
            if let Some(old) = recent.frames.pop_front() {
                recent.bytes -= old.data.len();
            }
            while recent.frames.front().is_some_and(|f| !f.key) {
                recent.bytes -= recent.frames.pop_front().unwrap().data.len();
            }
        }
    }
    pub fn snapshot(&self) -> Vec<Frame> {
        self.recent.lock().frames.iter().cloned().collect()
    }
    /// None means the overlap frame has been evicted. An empty result means
    /// playback is waiting for the next incoming frame.
    pub fn after(&self, absolute_ns: u64) -> Option<VecDeque<Frame>> {
        let relative = absolute_ns.checked_sub(self.origin_ms * 1_000_000)?;
        let recent = self.recent.lock();
        let index = recent.frames.partition_point(|f| f.pts_ns < relative);
        if recent.frames.get(index)?.pts_ns != relative {
            return None;
        }
        Some(
            recent
                .frames
                .iter()
                .skip(index + 1)
                .map(|frame| Frame {
                    pts_ns: self.origin_ms * 1_000_000 + frame.pts_ns,
                    ..frame.clone()
                })
                .collect(),
        )
    }
    pub fn contains(&self, timestamp: u64) -> bool {
        let recent = self.recent.lock();
        recent
            .frames
            .front()
            .zip(recent.frames.back())
            .is_some_and(|(first, last)| {
                timestamp >= self.origin_ms + first.pts_ns / 1_000_000
                    && timestamp <= self.origin_ms + last.pts_ns / 1_000_000
            })
    }
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

pub fn list_in(directory: &Path) -> Result<Vec<Segment>> {
    let mut segments = Vec::new();
    if !directory.exists() {
        return Ok(segments);
    }
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.extension().is_none_or(|s| s != "json") {
            continue;
        }
        let mut segment: Segment = serde_json::from_slice(&std::fs::read(&path)?)?;
        segment.path = path.with_extension("mp4");
        ensure!(
            segment.end_ns >= segment.first_ns,
            "Invalid DVR segment interval"
        );
        if segment.path.is_file() {
            segments.push(segment);
        }
    }
    segments.sort_by_key(|s| (s.origin_ms, s.first_ns));
    Ok(segments)
}

pub fn list(source: &VideoSourceId) -> Result<Vec<Segment>> {
    let root = super::filesystem::get_stream_dvr_dir(source);
    let mut result = Vec::new();
    if root.exists() {
        for entry in std::fs::read_dir(root)? {
            let path = entry?.path();
            if path.is_dir()
                && path
                    .file_name()
                    .is_some_and(|s| s.to_string_lossy().starts_with("segments-"))
            {
                result.extend(list_in(&path)?);
            }
        }
    }
    result.sort_by_key(Segment::start_ms);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_buffer_evicts_whole_gops_when_byte_budget_is_reached() {
        let source = format!("buffer-test-{}", uuid::Uuid::new_v4());
        let directory = std::env::temp_dir().join(&source);
        let session =
            Session::in_directory(&source, 0, VideoCodec::H264, directory.clone()).unwrap();
        let data = Arc::new(vec![0; 1024 * 1024]);
        for index in 0..100 {
            session.push(Frame {
                pts_ns: index * 1_000_000,
                key: index % 10 == 0,
                data: data.clone(),
            });
        }
        let recent = session.recent.lock();
        assert!(recent.bytes <= BUFFER_BYTES);
        assert!(recent.frames.front().unwrap().key);
        assert!(recent.frames.front().unwrap().pts_ns > 0);
        assert_eq!(recent.frames.back().unwrap().pts_ns, 99_000_000);
        drop(recent);
        std::fs::remove_dir(directory).unwrap();
    }
}
