//! One paced RTP timeline spanning finalized MP4 files and recent encoded video.
use crate::{
    app::VideoSourceId,
    common::{VideoCodec, nal_utils, rtp::RtpPacketizer, traits::RtpConsumer},
    domain::dvr::segments::{self, Frame, Segment, Session},
};
use anyhow::{Result, anyhow, bail, ensure};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const MAX_SEGMENT_BYTES: usize = 64 * 1024 * 1024;

fn absolute(session: &Session, frame: &Frame) -> Frame {
    Frame {
        pts_ns: session.origin_ms * 1_000_000 + frame.pts_ns,
        dts_ns: session.origin_ms * 1_000_000 + frame.dts_ns,
        ..frame.clone()
    }
}

// Expose only complete top-level MP4 boxes through the last complete mdat.
// This is a bounded byte view, not a synthetic MP4 index or footer.
fn readable_len(path: &std::path::Path) -> Result<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    if path.with_extension("json").exists() {
        return Ok(length);
    }
    let mut offset = 0u64;
    let mut complete = 0;
    while length.saturating_sub(offset) >= 8 {
        file.seek(SeekFrom::Start(offset))?;
        let mut header = [0u8; 8];
        file.read_exact(&mut header)?;
        let short = u32::from_be_bytes(header[..4].try_into()?);
        let (size, minimum) = if short == 1 {
            if length - offset < 16 {
                break;
            }
            let mut extended = [0u8; 8];
            file.read_exact(&mut extended)?;
            (u64::from_be_bytes(extended), 16)
        } else {
            (short as u64, 8)
        };
        if size < minimum || size > length - offset {
            break;
        }
        offset += size;
        if &header[4..] == b"mdat" {
            complete = offset;
        }
    }
    ensure!(
        complete > 0,
        "Active DVR file has no complete media fragment yet"
    );
    Ok(complete)
}

fn snapshot_source(path: &std::path::Path, size: u64) -> Result<gst::Element> {
    use std::io::{Read, Seek, SeekFrom};
    let file = Arc::new(parking_lot::Mutex::new(std::fs::File::open(path)?));
    let seek_file = file.clone();
    let source = gst_app::AppSrc::builder()
        .stream_type(gst_app::AppStreamType::RandomAccess)
        .format(gst::Format::Bytes)
        .size(i64::try_from(size)?)
        .build();
    source.set_callbacks(
        gst_app::AppSrcCallbacks::builder()
            .seek_data(move |_, offset| {
                offset <= size && seek_file.lock().seek(SeekFrom::Start(offset)).is_ok()
            })
            .need_data(move |source, requested| {
                let result = (|| -> Result<Vec<u8>> {
                    let mut file = file.lock();
                    let remaining = size.saturating_sub(file.stream_position()?);
                    let mut data =
                        vec![0; remaining.min(requested as u64).min(1024 * 1024) as usize];
                    file.read_exact(&mut data)?;
                    Ok(data)
                })();
                match result {
                    Ok(data) if data.is_empty() => {
                        let _ = source.end_of_stream();
                    }
                    Ok(data) => {
                        let _ = source.push_buffer(gst::Buffer::from_mut_slice(data));
                    }
                    Err(error) => gst::element_error!(
                        source,
                        gst::ResourceError::Read,
                        ("DVR read failed"),
                        ["{error:#}"]
                    ),
                }
            })
            .build(),
    );
    Ok(source.upcast())
}

/// A pull-based demuxer keeps only a bounded appsink queue and one batch in RAM.
struct SegmentReader {
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
    segment: Segment,
    first_pts: Option<u64>,
    opened_len: u64,
    exhausted: bool,
}
impl Drop for SegmentReader {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
impl SegmentReader {
    fn open(segment: Segment) -> Result<Self> {
        let codec = segment.codec()?;
        let pipeline = gst::Pipeline::new();
        let opened_len = readable_len(&segment.path)?;
        let src = if segment.path.with_extension("json").exists() {
            gst::ElementFactory::make("filesrc")
                .property("location", segment.path.to_string_lossy().as_ref())
                .build()?
        } else {
            snapshot_source(&segment.path, opened_len)?
        };
        let demux = gst::ElementFactory::make("qtdemux").build()?;
        let parser = gst::ElementFactory::make(match codec {
            VideoCodec::H264 => "h264parse",
            VideoCodec::H265 => "h265parse",
        })
        .property("config-interval", -1i32)
        .build()?;
        let caps = gst::Caps::builder(match codec {
            VideoCodec::H264 => "video/x-h264",
            VideoCodec::H265 => "video/x-h265",
        })
        .field("stream-format", "byte-stream")
        .field("alignment", "au")
        .build();
        let sink = gst_app::AppSink::builder()
            .caps(&caps)
            .sync(false)
            .max_buffers(8)
            .build();
        if sink.find_property("max-bytes").is_some() {
            sink.set_property("max-bytes", 4 * 1024 * 1024u64);
        }
        pipeline.add_many([&src, &demux, &parser, sink.upcast_ref()])?;
        src.link(&demux)?;
        parser.link(&sink)?;
        let parser_clone = parser.clone();
        demux.connect_pad_added(move |_, pad| {
            if let Some(sink) = parser_clone.static_pad("sink")
                && !sink.is_linked()
            {
                let _ = pad.link(&sink);
            }
        });
        pipeline.set_state(gst::State::Playing)?;
        Ok(Self {
            pipeline,
            sink,
            opened_len,
            segment,
            first_pts: None,
            exhausted: false,
        })
    }
    fn next(&mut self) -> Result<Option<Frame>> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(sample) = self
                .sink
                .try_pull_sample(gst::ClockTime::from_mseconds(100))
            {
                let buffer = sample
                    .buffer()
                    .ok_or_else(|| anyhow!("Missing DVR buffer"))?;
                let pts = buffer
                    .pts()
                    .ok_or_else(|| anyhow!("Missing DVR timestamp"))?
                    .nseconds();
                let first = *self.first_pts.get_or_insert(pts);
                let absolute = self.segment.origin_ms as i128 * 1_000_000
                    + self.segment.first_ns as i128
                    + pts as i128
                    - first as i128;
                let dts = buffer.dts().map(|t| t.nseconds()).unwrap_or(pts);
                let absolute_dts = self.segment.origin_ms as i128 * 1_000_000
                    + self.segment.first_ns as i128
                    + dts as i128
                    - first as i128;
                let data = buffer.map_readable()?;
                ensure!(
                    data.len() <= MAX_SEGMENT_BYTES,
                    "Single DVR frame exceeds memory budget"
                );
                return Ok(Some(Frame {
                    pts_ns: u64::try_from(absolute)?,
                    dts_ns: u64::try_from(absolute_dts)?,
                    key: segments::independent_frame(data.as_slice(), self.segment.codec()?),
                    data: Arc::new(data.as_slice().to_vec()),
                }));
            }
            if let Some(message) = self
                .pipeline
                .bus()
                .and_then(|bus| bus.pop_filtered(&[gst::MessageType::Error]))
                && let gst::MessageView::Error(error) = message.view()
            {
                bail!("DVR segment read failed: {}", error.error());
            }
            if self.sink.is_eos() {
                self.exhausted = true;
                return Ok(None);
            }
            ensure!(Instant::now() < deadline, "DVR segment read timed out");
        }
    }
    fn batch(&mut self) -> Result<VecDeque<Frame>> {
        let mut frames = VecDeque::new();
        let mut bytes = 0;
        while bytes < 4 * 1024 * 1024 && frames.len() < 64 {
            let Some(frame) = self.next()? else {
                break;
            };
            bytes += frame.data.len();
            frames.push_back(frame);
        }
        Ok(frames)
    }
}

// Only test/probe code materializes a whole file to compare all source frames.
#[cfg(test)]
pub(crate) fn read_segment(segment: Segment) -> Result<Vec<Frame>> {
    let mut reader = SegmentReader::open(segment)?;
    let mut result = Vec::new();
    while let Some(frame) = reader.next()? {
        result.push(frame);
    }
    ensure!(!result.is_empty(), "DVR segment has no video");
    Ok(result)
}

#[derive(Default)]
struct DiskCursor {
    reader: Option<SegmentReader>,
}
type SharedDisk = Arc<parking_lot::Mutex<DiskCursor>>;

async fn disk_batch(
    disk: SharedDisk,
    segment: Segment,
    target: Option<u64>,
) -> Result<VecDeque<Frame>> {
    tokio::task::spawn_blocking(move || {
        let mut cursor = disk.lock();
        if cursor.reader.as_ref().is_none_or(|r| {
            r.segment.path != segment.path
                || (r.exhausted
                    && readable_len(&segment.path).is_ok_and(|length| length != r.opened_len))
        }) {
            cursor.reader = Some(SegmentReader::open(segment)?);
        }
        let reader = cursor.reader.as_mut().unwrap();
        if let Some(target) = target {
            let mut anchor = None;
            while let Some(frame) = reader.next()? {
                if frame.key && frame.pts_ns <= target {
                    anchor = Some(frame.dts_ns);
                }
                if frame.dts_ns > target {
                    break;
                }
            }
            let anchor =
                anchor.ok_or_else(|| anyhow!("No independent decoding point in segment"))?;
            let segment = reader.segment.clone();
            *reader = SegmentReader::open(segment)?;
            while let Some(frame) = reader.next()? {
                if frame.dts_ns == anchor {
                    let mut frames = VecDeque::from([frame]);
                    frames.extend(reader.batch()?);
                    return Ok(frames);
                }
            }
            bail!("DVR decoding point disappeared");
        }
        reader.batch()
    })
    .await?
}

fn at_keyframe(frames: Vec<Frame>, target_ns: u64) -> Result<VecDeque<Frame>> {
    let index = frames
        .iter()
        .rposition(|f| f.key && f.pts_ns <= target_ns)
        .ok_or_else(|| anyhow!("No retained keyframe at requested DVR time"))?;
    Ok(frames.into_iter().skip(index).collect())
}

struct Prepared {
    frames: VecDeque<Frame>,
    session: Option<Arc<Session>>,
    codec: VideoCodec,
    disk: SharedDisk,
}

async fn prepare(source: &VideoSourceId, timestamp: u64) -> Result<Prepared> {
    let target = timestamp
        .checked_mul(1_000_000)
        .ok_or_else(|| anyhow!("Invalid timestamp"))?;
    let session = Session::active(source);
    let disk = Arc::new(parking_lot::Mutex::new(DiskCursor::default()));
    if let Some(recent) = &session
        && recent.contains(timestamp)
        && let Ok(frames) = at_keyframe(
            recent
                .snapshot()
                .iter()
                .map(|f| absolute(recent, f))
                .collect(),
            target,
        )
    {
        return Ok(Prepared {
            frames,
            codec: recent.codec,
            disk,
            session,
        });
    }

    let catalog = segments::list(source)?;
    let index = catalog
        .iter()
        .rposition(|s| s.start_ms() <= timestamp && s.end_ms() >= timestamp)
        .ok_or_else(|| anyhow!("No segmented recording at requested time"))?;
    let selected = &catalog[index];
    let codec = selected.codec()?;
    let mut prepared_frames = None;
    for segment in catalog[..=index].iter().rev() {
        if segment.origin_ms != selected.origin_ms || segment.codec()? != codec {
            break;
        }
        // Search earlier files when an open-GOP file has no independent start.
        *disk.lock() = DiskCursor::default();
        match disk_batch(disk.clone(), segment.clone(), Some(target)).await {
            Ok(frames) => {
                prepared_frames = Some(frames);
                break;
            }
            Err(error) if error.to_string() == "No independent decoding point in segment" => {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    let frames =
        prepared_frames.ok_or_else(|| anyhow!("No retained independent decoding point"))?;
    Ok(Prepared {
        frames,
        session,
        codec,
        disk,
    })
}

/// Prefer memory only when it overlaps our exact last frame. This prevents gaps
/// and duplicates when a finalized file and the recent buffer cover the same GOP.
async fn next_frames(
    source: &VideoSourceId,
    session: Option<&Arc<Session>>,
    last: u64,
    codec: VideoCodec,
    disk: SharedDisk,
) -> Result<VecDeque<Frame>> {
    if let Some(session) = session
        && let Some(frames) = session.after(last)
    {
        return Ok(frames);
    }
    let cached = disk.lock().reader.as_ref().map(|r| r.segment.clone());
    if let Some(segment) = cached {
        loop {
            let frames = disk_batch(disk.clone(), segment.clone(), None).await?;
            if frames.is_empty() {
                break;
            }
            let frames: VecDeque<_> = frames.into_iter().filter(|f| f.dts_ns > last).collect();
            if !frames.is_empty() {
                return Ok(frames);
            }
        }
    }
    for segment in segments::list(source)? {
        if segment.origin_ms * 1_000_000 + segment.end_ns < last {
            continue;
        }
        ensure!(
            segment.codec()? == codec,
            "DVR codec changed between recordings"
        );
        let frames = disk_batch(disk.clone(), segment, None).await?;
        let frames: VecDeque<_> = frames.into_iter().filter(|f| f.dts_ns > last).collect();
        if !frames.is_empty() {
            return Ok(frames);
        }
    }
    Ok(VecDeque::new())
}

struct Playback {
    task: Option<tokio::task::JoinHandle<()>>,
    prepared: Option<Prepared>,
}

pub struct SegmentedPlayer {
    source: VideoSourceId,
    consumer: Arc<dyn RtpConsumer>,
    state: tokio::sync::Mutex<Playback>,
    position_ns: Arc<AtomicU64>,
}
impl Drop for SegmentedPlayer {
    fn drop(&mut self) {
        if let Some(task) = self.state.get_mut().task.take() {
            task.abort();
        }
    }
}
impl SegmentedPlayer {
    pub fn available(source: &VideoSourceId, timestamp: u64) -> bool {
        Session::active(source).is_some_and(|session| session.contains(timestamp))
            || segments::list(source).is_ok_and(|catalog| {
                catalog
                    .iter()
                    .any(|segment| segment.start_ms() <= timestamp && timestamp <= segment.end_ms())
            })
    }
    pub fn speed(&self) -> f64 {
        1.0
    }
    pub fn set_speed(&self, _speed: f64) {
        log::warn!("set_speed not fully implemented yet");
    }
    pub async fn new(
        source: VideoSourceId,
        timestamp: u64,
        consumer: Arc<dyn RtpConsumer>,
    ) -> Result<Self> {
        let prepared = prepare(&source, timestamp).await?;
        let actual_ns = prepared.frames.front().unwrap().pts_ns;
        Ok(Self {
            source,
            consumer,
            state: tokio::sync::Mutex::new(Playback {
                task: None,
                prepared: Some(prepared),
            }),
            position_ns: Arc::new(AtomicU64::new(actual_ns)),
        })
    }
    fn spawn(&self, prepared: Prepared) -> tokio::task::JoinHandle<()> {
        let source = self.source.clone();
        let consumer = self.consumer.clone();
        let position = self.position_ns.clone();
        tokio::spawn(async move {
            if let Err(error) = stream(source, consumer, position, prepared).await {
                log::error!("Segmented DVR playback stopped: {error:#}");
            }
        })
    }
    pub async fn play(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        ensure!(state.task.is_none(), "DVR playback is already running");
        let prepared = state
            .prepared
            .take()
            .ok_or_else(|| anyhow!("DVR playback has no prepared video"))?;
        self.consumer.on_source_switch();
        state.task = Some(self.spawn(prepared));
        Ok(())
    }
    pub async fn terminate(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        if let Some(task) = state.task.take() {
            task.abort();
            let _ = task.await;
        }
        Ok(())
    }
    pub async fn seek_to_timestamp(&self, timestamp: u64, _speed: f64) -> Result<()> {
        let mut state = self.state.lock().await;
        let prepared = prepare(&self.source, timestamp).await?;
        if let Some(task) = state.task.take() {
            task.abort();
            let _ = task.await;
        }
        self.consumer.on_source_switch();
        self.position_ns
            .store(prepared.frames.front().unwrap().pts_ns, Ordering::Release);
        state.task = Some(self.spawn(prepared));
        Ok(())
    }
    pub async fn current_timestamp(&self) -> u64 {
        self.position_ns.load(Ordering::Acquire) / 1_000_000
    }
}

// Aborting playback must also cancel its lookahead task.
struct Prefetch(tokio::task::JoinHandle<Result<VecDeque<Frame>>>);
impl Drop for Prefetch {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn stream(
    source: VideoSourceId,
    consumer: Arc<dyn RtpConsumer>,
    position: Arc<AtomicU64>,
    mut prepared: Prepared,
) -> Result<()> {
    let packetizer = RtpPacketizer::new(rand::random(), 96);
    let first = prepared
        .frames
        .front()
        .ok_or_else(|| anyhow!("No DVR frames"))?
        .dts_ns;
    let started = tokio::time::Instant::now();
    let mut empty_since = None;
    loop {
        let last = prepared
            .frames
            .back()
            .ok_or_else(|| anyhow!("No DVR frames"))?
            .dts_ns;
        // Start fetching the next file while the current batch is being paced.
        let next_source = source.clone();
        let session = prepared.session.clone();
        let codec = prepared.codec;
        let disk = prepared.disk.clone();
        let mut next = Prefetch(tokio::spawn(async move {
            next_frames(&next_source, session.as_ref(), last, codec, disk).await
        }));
        while let Some(frame) = prepared.frames.pop_front() {
            tokio::time::sleep_until(
                started + Duration::from_nanos(frame.dts_ns.saturating_sub(first)),
            )
            .await;
            let nals = nal_utils::parse_annex_b(&frame.data);
            let timestamp = ((frame.pts_ns as u128 * 90000) / 1_000_000_000) as u32;
            for packet in packetizer.packetize(&nals, timestamp, &prepared.codec) {
                consumer.on_new_packet(Arc::new(packet)).await;
            }
            position.store(frame.pts_ns, Ordering::Release);
        }
        loop {
            prepared.frames = (&mut next.0).await??;
            if !prepared.frames.is_empty() {
                empty_since = None;
                break;
            }
            if prepared
                .session
                .as_ref()
                .is_none_or(|s| s.closed.load(Ordering::Acquire))
            {
                return Ok(());
            }
            let waiting = empty_since.get_or_insert_with(Instant::now);
            ensure!(
                waiting.elapsed() < Duration::from_secs(30),
                "Recent DVR frames unavailable for thirty seconds"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
            let next_source = source.clone();
            let session = prepared.session.clone();
            let codec = prepared.codec;
            let disk = prepared.disk.clone();
            next = Prefetch(tokio::spawn(async move {
                next_frames(&next_source, session.as_ref(), last, codec, disk).await
            }));
        }
    }
}

#[cfg(test)]
mod active_file_tests {
    use super::*;
    #[test]
    fn partial_media_box_is_not_exposed_to_the_demuxer() {
        gst::init().unwrap();
        let bytes = include_bytes!("../../tests/fixtures/dvr-fragmented.mp4");
        let path = std::env::temp_dir().join(format!("partial-mp4-{}.mp4", uuid::Uuid::new_v4()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = Cleanup(path.clone());
        std::fs::write(&path, &bytes[..bytes.len() - 7]).unwrap();
        let segment = Segment {
            origin_ms: 0,
            first_ns: 0,
            end_ns: 12_000_000_000,
            codec: "h264".into(),
            path: path.clone(),
        };
        let prefix = readable_len(&path).unwrap();
        assert!(prefix < (bytes.len() - 7) as u64);
        let frames = read_segment(segment.clone()).unwrap();
        assert_eq!(frames.len(), 330);
        std::fs::write(&path, bytes).unwrap();
        assert!(readable_len(&path).unwrap() > prefix);
        assert_eq!(read_segment(segment).unwrap().len(), 360);
    }
}
