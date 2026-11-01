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
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

pub const SEGMENT_NS: u64 = 10_000_000_000;
const BUFFER_NS: u64 = 30_000_000_000;
const BUFFER_BYTES: usize = 64 * 1024 * 1024;

/// Conservative random-access points: H.264 IDR and H.265 IDR/BLA.
/// CRA/recovery-point pictures can require leading-picture/dependency handling.
pub fn independent_frame(data: &[u8], codec: VideoCodec) -> bool {
    crate::common::nal_utils::parse_annex_b(data)
        .iter()
        .any(|nal| {
            if nal.is_empty() {
                return false;
            }
            match codec {
                VideoCodec::H264 => nal[0] & 0x1f == 5,
                VideoCodec::H265 => nal.len() >= 2 && (16..=20).contains(&((nal[0] >> 1) & 0x3f)),
            }
        })
}

#[derive(Clone)]
pub struct Frame {
    pub pts_ns: u64,
    pub dts_ns: u64,
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
    pub fn validate(&self) -> Result<()> {
        ensure!(self.end_ns >= self.first_ns, "Invalid DVR segment interval");
        ensure!(
            self.origin_ms
                .checked_mul(1_000_000)
                .and_then(|origin| origin.checked_add(self.end_ns))
                .is_some(),
            "DVR segment timestamp overflow"
        );
        self.codec()?;
        Ok(())
    }
    pub fn start_ms(&self) -> u64 {
        self.origin_ms + self.first_ns / 1_000_000
    }
    pub fn end_ms(&self) -> u64 {
        self.origin_ms + self.end_ns / 1_000_000
    }
    pub fn journal(&self) -> Result<()> {
        let pending = self.path.with_extension("pending");
        let temporary = self.path.with_extension("pending.tmp");
        std::fs::write(&temporary, serde_json::to_vec(self)?)?;
        std::fs::rename(temporary, pending)?;
        Ok(())
    }
    pub fn publish(&self) -> Result<()> {
        let path = self.path.with_extension("json");
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec(self)?)?;
        std::fs::rename(temporary, path)?;
        match std::fs::remove_file(self.path.with_extension("pending")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
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
    pub next_file: AtomicU64,
    pub first_dts: AtomicU64,
    pub completed_until: AtomicU64,
    pub active_files: Mutex<Vec<Segment>>,
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
            next_file: AtomicU64::new(0),
            first_dts: AtomicU64::new(u64::MAX),
            completed_until: AtomicU64::new(0),
            active_files: Mutex::new(Vec::new()),
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
        let _ = self.first_dts.compare_exchange(
            u64::MAX,
            frame.dts_ns,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
        recent.bytes += frame.data.len();
        let newest = frame.pts_ns;
        recent.frames.push_back(frame);
        self.trim_recent(&mut recent, newest);
    }

    pub(crate) fn trim_completed(&self) {
        let mut recent = self.recent.lock();
        if let Some(newest) = recent.frames.back().map(|frame| frame.pts_ns) {
            self.trim_recent(&mut recent, newest);
        }
    }

    fn trim_recent(&self, recent: &mut Recent, newest: u64) {
        // The duration is a soft target: keep the current GOP until another
        // decoding anchor exists. The byte limit remains a hard bound.
        loop {
            let next_key = recent
                .frames
                .iter()
                .skip(1)
                .position(|f| f.key)
                .map(|i| i + 1);
            let expired = recent
                .frames
                .front()
                .is_some_and(|f| newest.saturating_sub(f.pts_ns) > BUFFER_NS);
            let on_disk = next_key.is_some_and(|index| {
                self.completed_until.load(Ordering::Acquire) >= recent.frames[index - 1].dts_ns
            });
            if recent.bytes <= BUFFER_BYTES && !(expired && on_disk) {
                break;
            }
            let remove = next_key.unwrap_or(recent.frames.len());
            for _ in 0..remove {
                if let Some(old) = recent.frames.pop_front() {
                    recent.bytes -= old.data.len();
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn clear_recent_for_test(&self) {
        *self.recent.lock() = Recent::default();
    }
    pub fn snapshot(&self) -> Vec<Frame> {
        self.recent.lock().frames.iter().cloned().collect()
    }
    /// None means the overlap frame has been evicted. An empty result means
    /// playback is waiting for the next incoming frame.
    pub fn after(&self, absolute_ns: u64) -> Option<VecDeque<Frame>> {
        let relative = absolute_ns.checked_sub(self.origin_ms * 1_000_000)?;
        let recent = self.recent.lock();
        let index = recent.frames.partition_point(|f| f.dts_ns < relative);
        if recent.frames.get(index)?.dts_ns != relative {
            return None;
        }
        Some(
            recent
                .frames
                .iter()
                .skip(index + 1)
                .map(|frame| Frame {
                    pts_ns: self.origin_ms * 1_000_000 + frame.pts_ns,
                    dts_ns: self.origin_ms * 1_000_000 + frame.dts_ns,
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

pub fn active_paths() -> std::collections::HashSet<PathBuf> {
    sessions()
        .lock()
        .values()
        .filter_map(Weak::upgrade)
        .flat_map(|session| {
            session
                .active_files
                .lock()
                .iter()
                .map(|s| s.path.clone())
                .collect::<Vec<_>>()
        })
        .collect()
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
        segment.validate()?;
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
    if let Some(session) = Session::active(source) {
        for segment in session.active_files.lock().iter() {
            if !result.iter().any(|s| s.path == segment.path)
                && segment.path.metadata().is_ok_and(|m| m.len() > 0)
            {
                result.push(segment.clone());
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
                dts_ns: index * 1_000_000,
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
    #[test]
    fn duration_target_keeps_a_long_active_gop_within_byte_budget() {
        let source = format!("long-gop-{}", uuid::Uuid::new_v4());
        let directory = std::env::temp_dir().join(&source);
        let session =
            Session::in_directory(&source, 0, VideoCodec::H264, directory.clone()).unwrap();
        for index in 0..450 {
            session.push(Frame {
                pts_ns: index * 100_000_000,
                dts_ns: index * 100_000_000,
                key: index == 0,
                data: Arc::new(vec![1; 100]),
            });
        }
        assert!(session.contains(44_000));
        assert_eq!(session.snapshot().first().unwrap().pts_ns, 0);
        session.push(Frame {
            pts_ns: 45_000_000_000,
            dts_ns: 45_000_000_000,
            key: true,
            data: Arc::new(vec![1; 100]),
        });
        assert_eq!(session.snapshot().first().unwrap().pts_ns, 0);
        session
            .completed_until
            .store(44_900_000_000, Ordering::Release);
        session.push(Frame {
            pts_ns: 45_100_000_000,
            dts_ns: 45_100_000_000,
            key: false,
            data: Arc::new(vec![1; 100]),
        });
        assert_eq!(session.snapshot().first().unwrap().pts_ns, 45_000_000_000);
        std::fs::remove_dir(directory).unwrap();
    }
}
