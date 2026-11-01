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
        ..frame.clone()
    }
}

/// Read encoded access units, without decoding or re-encoding the video.
pub(crate) fn read_segment(segment: Segment) -> Result<Vec<Frame>> {
    let codec = segment.codec()?;
    let pipeline = gst::Pipeline::new();
    struct Stop(gst::Pipeline);
    impl Drop for Stop {
        fn drop(&mut self) {
            let _ = self.0.set_state(gst::State::Null);
        }
    }
    let _stop = Stop(pipeline.clone());
    let src = gst::ElementFactory::make("filesrc")
        .property("location", segment.path.to_string_lossy().as_ref())
        .build()?;
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
    let mut frames = Vec::new();
    let mut bytes = 0;
    let mut first_pts = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            let buffer = sample
                .buffer()
                .ok_or_else(|| anyhow!("Missing DVR sample buffer"))?;
            let pts = buffer
                .pts()
                .ok_or_else(|| anyhow!("Missing DVR sample timestamp"))?
                .nseconds();
            let first = *first_pts.get_or_insert(pts);
            ensure!(
                pts >= first,
                "DVR presentation timestamps are not monotonic"
            );
            let map = buffer.map_readable()?;
            bytes += map.len();
            ensure!(
                bytes <= MAX_SEGMENT_BYTES,
                "DVR segment exceeds playback memory limit"
            );
            frames.push(Frame {
                pts_ns: segment.origin_ms * 1_000_000 + segment.first_ns + pts - first,
                key: !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT),
                data: Arc::new(map.as_slice().to_vec()),
            });
        } else if sink.is_eos() {
            break;
        } else {
            if let Some(message) = pipeline
                .bus()
                .and_then(|bus| bus.pop_filtered(&[gst::MessageType::Error]))
                && let gst::MessageView::Error(error) = message.view()
            {
                bail!("DVR segment read failed: {}", error.error());
            }
            ensure!(Instant::now() < deadline, "DVR segment read timed out");
        }
    }
    ensure!(!frames.is_empty(), "DVR segment has no video");
    Ok(frames)
}

async fn disk_frames(segment: Segment) -> Result<Vec<Frame>> {
    tokio::task::spawn_blocking(move || read_segment(segment)).await?
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
}

async fn prepare(source: &VideoSourceId, timestamp: u64) -> Result<Prepared> {
    let target = timestamp
        .checked_mul(1_000_000)
        .ok_or_else(|| anyhow!("Invalid timestamp"))?;
    let session = Session::active(source);
    if let Some(recent) = &session
        && recent.contains(timestamp)
    {
        let frames = at_keyframe(
            recent
                .snapshot()
                .iter()
                .map(|f| absolute(recent, f))
                .collect(),
            target,
        )?;
        return Ok(Prepared {
            frames,
            codec: recent.codec,
            session,
        });
    }
    let catalog = segments::list(source)?;
    let segment = catalog
        .into_iter()
        .rev()
        .find(|s| s.start_ms() <= timestamp && s.end_ms() >= timestamp)
        .ok_or_else(|| anyhow!("No segmented recording at requested time"))?;
    let codec = segment.codec()?;
    let frames = at_keyframe(disk_frames(segment).await?, target)?;
    Ok(Prepared {
        frames,
        session,
        codec,
    })
}

/// Prefer memory only when it overlaps our exact last frame. This prevents gaps
/// and duplicates when a finalized file and the recent buffer cover the same GOP.
async fn next_frames(
    source: &VideoSourceId,
    session: Option<&Arc<Session>>,
    last: u64,
    codec: VideoCodec,
) -> Result<VecDeque<Frame>> {
    if let Some(session) = session
        && let Some(frames) = session.after(last)
    {
        return Ok(frames);
    }
    for segment in segments::list(source)? {
        if segment.origin_ms * 1_000_000 + segment.end_ns < last {
            continue;
        }
        ensure!(
            segment.codec()? == codec,
            "DVR codec changed between recordings"
        );
        let frames = disk_frames(segment).await?;
        let frames: VecDeque<_> = frames.into_iter().filter(|f| f.pts_ns > last).collect();
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
    pub async fn new(
        source: VideoSourceId,
        timestamp: u64,
        consumer: Arc<dyn RtpConsumer>,
    ) -> Result<Self> {
        let prepared = prepare(&source, timestamp).await?;
        Ok(Self {
            source,
            consumer,
            state: tokio::sync::Mutex::new(Playback {
                task: None,
                prepared: Some(prepared),
            }),
            position_ns: Arc::new(AtomicU64::new(timestamp * 1_000_000)),
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
            .store(timestamp * 1_000_000, Ordering::Release);
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
        .pts_ns;
    let started = tokio::time::Instant::now();
    let mut empty_since = None;
    loop {
        let last = prepared
            .frames
            .back()
            .ok_or_else(|| anyhow!("No DVR frames"))?
            .pts_ns;
        // Start fetching the next file while the current batch is being paced.
        let next_source = source.clone();
        let session = prepared.session.clone();
        let codec = prepared.codec;
        let mut next = Prefetch(tokio::spawn(async move {
            next_frames(&next_source, session.as_ref(), last, codec).await
        }));
        while let Some(frame) = prepared.frames.pop_front() {
            tokio::time::sleep_until(
                started + Duration::from_nanos(frame.pts_ns.saturating_sub(first)),
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
            next = Prefetch(tokio::spawn(async move {
                next_frames(&next_source, session.as_ref(), last, codec).await
            }));
        }
    }
}
