//! One paced RTP timeline spanning finalized MP4 files and recent encoded video.
#[cfg(test)]
pub(crate) use crate::domain::dvr::reader::read_segment;
use crate::domain::dvr::reader::{SegmentReader, readable_len};
use crate::{
    app::VideoSourceId,
    common::{VideoCodec, nal_utils, rtp::RtpPacketizer, traits::RtpConsumer},
    domain::dvr::segments::{self, Frame, Segment, Session},
};
use anyhow::{Result, anyhow, bail, ensure};
use media_server_api_models::PlaybackEvent;
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

fn absolute(session: &Session, frame: &Frame) -> Frame {
    Frame {
        pts_ns: session.origin_ms * 1_000_000 + frame.pts_ns,
        dts_ns: session.origin_ms * 1_000_000 + frame.dts_ns,
        ..frame.clone()
    }
}

#[derive(Default)]
struct DiskCursor {
    reader: Option<SegmentReader>,
    codec: Option<VideoCodec>,
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
            cursor.reader = Some(SegmentReader::open(segment.clone())?);
        }
        cursor.codec = Some(segment.codec()?);
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
    seek_gap: Option<(u64, u64)>,
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
            seek_gap: None,
        });
    }

    let catalog = segments::list(source)?;
    let index = catalog
        .iter()
        .rposition(|s| s.start_ms() <= timestamp && s.end_ms() >= timestamp)
        .or_else(|| catalog.iter().position(|s| s.start_ms() > timestamp))
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
        let scan_target = target.max(selected.origin_ms * 1_000_000 + selected.first_ns);
        match disk_batch(disk.clone(), segment.clone(), Some(scan_target)).await {
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
    let actual = frames.front().unwrap().pts_ns / 1_000_000;
    Ok(Prepared {
        frames,
        session,
        codec,
        disk,
        seek_gap: (actual > timestamp && selected.start_ms() > timestamp)
            .then_some((timestamp, actual)),
    })
}

/// Prefer memory only when it overlaps our exact last frame. This prevents gaps
/// and duplicates when a finalized file and the recent buffer cover the same GOP.
async fn next_frames(
    source: &VideoSourceId,
    session: Option<&Arc<Session>>,
    last: u64,
    _codec: VideoCodec,
    disk: SharedDisk,
) -> Result<VecDeque<Frame>> {
    if let Some(session) = session
        && let Some(frames) = session.after(last)
        && (!frames.is_empty() || !session.closed.load(Ordering::Acquire))
    {
        disk.lock().codec = Some(session.codec);
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
        let frames = match disk_batch(disk.clone(), segment, None).await {
            Ok(frames) => frames,
            Err(error)
                if error
                    .downcast_ref::<crate::domain::dvr::reader::NotReady>()
                    .is_some() =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        let frames: VecDeque<_> = frames.into_iter().filter(|f| f.dts_ns > last).collect();
        if !frames.is_empty() {
            return Ok(frames);
        }
    }
    // A new source session can already be in RAM before its first fragment
    // closes. Use it only if its original independent start is still retained.
    if let Some(active) = Session::active(source) {
        let frames = active.snapshot();
        if let Some(first) = frames.first()
            && first.key
            && first.dts_ns == active.first_dts.load(Ordering::Acquire)
            && active.origin_ms * 1_000_000 + first.dts_ns > last
        {
            disk.lock().codec = Some(active.codec);
            return Ok(frames.iter().map(|f| absolute(&active, f)).collect());
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
    decode_position: Arc<AtomicU64>,
    status: Arc<parking_lot::Mutex<PlaybackEvent>>,
    rate: tokio::sync::watch::Sender<f64>,
    caught_up: Arc<AtomicBool>,
    catch_up_ready: Arc<tokio::sync::Notify>,
}
impl Drop for SegmentedPlayer {
    fn drop(&mut self) {
        if let Some(task) = self.state.get_mut().task.take() {
            task.abort();
        }
    }
}
impl SegmentedPlayer {
    pub async fn codec_parameters(
        source: &VideoSourceId,
        timestamp: u64,
    ) -> Result<crate::common::rtp::CodecParameters> {
        let prepared = prepare(source, timestamp).await?;
        Ok(parameters(prepared.frames.front().unwrap(), prepared.codec))
    }
    pub fn available(source: &VideoSourceId, timestamp: u64) -> bool {
        Session::active(source).is_some_and(|session| session.contains(timestamp))
            || segments::list(source)
                .is_ok_and(|catalog| catalog.iter().any(|segment| timestamp <= segment.end_ms()))
    }
    pub fn speed(&self) -> f64 {
        *self.rate.borrow()
    }
    pub fn set_speed(&self, speed: f64) -> Result<()> {
        ensure!(
            speed.is_finite() && (0.25..=4.0).contains(&speed),
            "unsupported playback speed: expected a finite rate between 0.25 and 4.0"
        );
        self.caught_up.store(false, Ordering::Release);
        self.rate.send_replace(speed);
        Ok(())
    }
    pub async fn catch_up_requested(&self) {
        self.catch_up_ready.notified().await;
    }
    pub fn take_catch_up(&self) -> bool {
        if !self.caught_up.swap(false, Ordering::AcqRel) || self.speed() <= 1.0 {
            return false;
        }
        Session::active(&self.source).is_some_and(|active| {
            !active.closed.load(Ordering::Acquire)
                && active.latest_decode_time().is_some_and(|(head, arrived)| {
                    arrived.elapsed() < Duration::from_secs(1)
                        && head.abs_diff(self.decode_position.load(Ordering::Acquire))
                            <= 100_000_000
                })
        })
    }
    pub async fn require_live_codec(&self, codec: VideoCodec) {
        let event = PlaybackEvent::RenegotiationRequired {
            codec: codec.mime_type().into(),
            message: "Create a new WebRTC offer/session for the live codec".into(),
        };
        if *self.status.lock() != event {
            self.terminate().await.ok();
            notify(&self.consumer, &self.status, event).await;
        }
    }
    pub fn playback_event(&self) -> PlaybackEvent {
        self.status.lock().clone()
    }
    pub async fn new(
        source: VideoSourceId,
        timestamp: u64,
        consumer: Arc<dyn RtpConsumer>,
    ) -> Result<Self> {
        let prepared = prepare(&source, timestamp).await?;
        check_codec(&consumer, prepared.codec).await?;
        let actual_ns = prepared.frames.front().unwrap().pts_ns;
        Ok(Self {
            rate: tokio::sync::watch::channel(1.0).0,
            caught_up: Arc::new(AtomicBool::new(false)),
            catch_up_ready: Arc::new(tokio::sync::Notify::new()),
            source,
            consumer,
            state: tokio::sync::Mutex::new(Playback {
                task: None,
                prepared: Some(prepared),
            }),
            position_ns: Arc::new(AtomicU64::new(actual_ns)),
            decode_position: Arc::new(AtomicU64::new(0)),
            status: Arc::new(parking_lot::Mutex::new(PlaybackEvent::Playing {
                timestamp: actual_ns / 1_000_000,
            })),
        })
    }
    fn spawn(&self, prepared: Prepared) -> tokio::task::JoinHandle<()> {
        let source = self.source.clone();
        let consumer = self.consumer.clone();
        let position = self.position_ns.clone();
        let status = self.status.clone();
        let rate = self.rate.subscribe();
        let caught_up = self.caught_up.clone();
        let catch_up_ready = self.catch_up_ready.clone();
        let decode_position = self.decode_position.clone();
        tokio::spawn(async move {
            if let Err(error) = stream(
                source,
                consumer.clone(),
                position.clone(),
                status.clone(),
                prepared,
                rate,
                caught_up,
                decode_position,
                catch_up_ready,
            )
            .await
            {
                let event = PlaybackEvent::Error {
                    timestamp: position.load(Ordering::Acquire) / 1_000_000,
                    message: error.to_string(),
                };
                *status.lock() = event.clone();
                consumer.on_playback_event(event).await;
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
    pub async fn seek_to_timestamp(&self, timestamp: u64) -> Result<()> {
        let mut state = self.state.lock().await;
        let prepared = prepare(&self.source, timestamp).await?;
        check_codec(&self.consumer, prepared.codec).await?;
        if let Some(task) = state.task.take() {
            task.abort();
            let _ = task.await;
        }
        self.caught_up.store(false, Ordering::Release);
        self.consumer.on_source_switch();
        let timestamp = prepared.frames.front().unwrap().pts_ns / 1_000_000;
        self.position_ns
            .store(prepared.frames.front().unwrap().pts_ns, Ordering::Release);
        *self.status.lock() = PlaybackEvent::Playing { timestamp };
        state.task = Some(self.spawn(prepared));
        Ok(())
    }
    pub async fn current_timestamp(&self) -> u64 {
        self.position_ns.load(Ordering::Acquire) / 1_000_000
    }
}

/// Maps source decode/presentation time onto a continuous 90 kHz transport clock.
struct PlaybackClock {
    origin: u64,
    rtp_base: u32,
    source: f64,
    output: f64,
    wall: tokio::time::Instant,
    rate: f64,
}
impl PlaybackClock {
    fn new(source: u64, rate: f64) -> Self {
        Self {
            origin: source,
            rtp_base: ((source as u128 * 90000) / 1_000_000_000) as u32,
            source: 0.0,
            output: ((source as u128 * 90000) % 1_000_000_000) as f64 / 90000.0,
            wall: tokio::time::Instant::now(),
            rate,
        }
    }
    fn deadline(&self, dts: u64) -> tokio::time::Instant {
        self.wall
            + Duration::from_secs_f64(
                (((dts as i128 - self.origin as i128) as f64 - self.source) / self.rate / 1e9)
                    .max(0.0),
            )
    }
    fn timestamp(&self, pts: u64) -> u32 {
        let ticks = ((self.output
            + ((pts as i128 - self.origin as i128) as f64 - self.source) / self.rate)
            * 90000.0
            / 1e9)
            .floor() as i128;
        self.rtp_base
            .wrapping_add(ticks.rem_euclid(1i128 << 32) as u32)
    }
    fn set_rate(&mut self, rate: f64) {
        let elapsed = self.wall.elapsed().as_secs_f64() * 1e9;
        self.source += elapsed * self.rate;
        self.output += elapsed;
        self.wall = tokio::time::Instant::now();
        self.rate = rate;
    }
    fn resume(&mut self, dts: u64) {
        self.output += ((dts as i128 - self.origin as i128) as f64 - self.source) / self.rate;
        self.source = (dts as i128 - self.origin as i128) as f64;
        self.wall = tokio::time::Instant::now();
    }
}

// Aborting playback must also cancel its lookahead task.
struct Prefetch(tokio::task::JoinHandle<Result<VecDeque<Frame>>>);
impl Drop for Prefetch {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn check_codec(consumer: &Arc<dyn RtpConsumer>, codec: VideoCodec) -> Result<()> {
    if !consumer.accepts_codec(codec) {
        consumer
            .on_playback_event(PlaybackEvent::RenegotiationRequired {
                codec: codec.mime_type().into(),
                message: "Create a new WebRTC offer/session for this codec".into(),
            })
            .await;
        bail!(
            "renegotiation required for {}: create a new WebRTC offer/session",
            codec.mime_type()
        );
    }
    Ok(())
}
async fn notify(
    consumer: &Arc<dyn RtpConsumer>,
    status: &parking_lot::Mutex<PlaybackEvent>,
    event: PlaybackEvent,
) {
    *status.lock() = event.clone();
    consumer.on_playback_event(event).await;
}
fn parameters(frame: &Frame, codec: VideoCodec) -> crate::common::rtp::CodecParameters {
    let mut params = crate::common::rtp::CodecParameters::new(&codec);
    for nal in nal_utils::parse_annex_b(&frame.data) {
        params.update_from_nal(&nal);
    }
    params
}
async fn stream(
    source: VideoSourceId,
    consumer: Arc<dyn RtpConsumer>,
    position: Arc<AtomicU64>,
    status: Arc<parking_lot::Mutex<PlaybackEvent>>,
    mut prepared: Prepared,
    mut rate: tokio::sync::watch::Receiver<f64>,
    caught_up: Arc<AtomicBool>,
    decode_position: Arc<AtomicU64>,
    catch_up_ready: Arc<tokio::sync::Notify>,
) -> Result<()> {
    let packetizer = RtpPacketizer::new(rand::random(), 96);
    let first = prepared
        .frames
        .front()
        .ok_or_else(|| anyhow!("No DVR frames"))?
        .dts_ns;
    let mut clock = PlaybackClock::new(first, *rate.borrow());

    let mut last_sent = None;
    let mut configured = None;
    let mut configured_params = None;
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
            if configured != Some(prepared.codec) {
                if !consumer.accepts_codec(prepared.codec) {
                    notify(
                        &consumer,
                        &status,
                        PlaybackEvent::RenegotiationRequired {
                            codec: prepared.codec.mime_type().into(),
                            message: "Create a new WebRTC offer/session for this codec".into(),
                        },
                    )
                    .await;
                    return Ok(());
                }
                ensure!(frame.key, "Codec transition requires an independent frame");
                if configured.is_some() {
                    consumer.on_source_switch();
                }
                configured = Some(prepared.codec);
            }
            if frame.key {
                let params = parameters(&frame, prepared.codec);
                let ready = match &params {
                    crate::common::rtp::CodecParameters::H264 { sps, pps, .. } => {
                        sps.is_some() && pps.is_some()
                    }
                    crate::common::rtp::CodecParameters::H265 { vps, sps, pps, .. } => {
                        vps.is_some() && sps.is_some() && pps.is_some()
                    }
                };
                ensure!(ready, "Independent DVR frame is missing codec parameters");
                if configured_params.as_ref() != Some(&params) {
                    if configured_params.as_ref().is_some_and(
                        |old: &crate::common::rtp::CodecParameters| old.codec() == prepared.codec,
                    ) {
                        consumer.on_source_switch();
                    }
                    consumer.update_params(&params);
                    configured_params = Some(params);
                }
            }
            if let Some((from, to)) = prepared.seek_gap.take() {
                notify(&consumer, &status, PlaybackEvent::GapSkipped { from, to }).await;
            }
            if let Some(previous) = last_sent
                && frame.dts_ns.saturating_sub(previous) > 1_000_000_000
            {
                if !frame.key {
                    if !matches!(*status.lock(), PlaybackEvent::Waiting { .. }) {
                        notify(
                            &consumer,
                            &status,
                            PlaybackEvent::Waiting {
                                timestamp: position.load(Ordering::Acquire) / 1_000_000,
                            },
                        )
                        .await;
                    }
                    continue;
                }
                consumer.on_source_switch();
                notify(
                    &consumer,
                    &status,
                    PlaybackEvent::GapSkipped {
                        from: position.load(Ordering::Acquire) / 1_000_000,
                        to: frame.pts_ns / 1_000_000,
                    },
                )
                .await;
                clock.resume(frame.dts_ns);
            }
            if !matches!(*status.lock(), PlaybackEvent::Playing { .. }) || last_sent.is_none() {
                notify(
                    &consumer,
                    &status,
                    PlaybackEvent::Playing {
                        timestamp: frame.pts_ns / 1_000_000,
                    },
                )
                .await;
            }
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(clock.deadline(frame.dts_ns)) => break,
                    changed = rate.changed() => {
                        if changed.is_err() { return Ok(()); }
                        clock.set_rate(*rate.borrow_and_update());
                    }
                }
            }
            // Bound recovery after downstream stalls; never drain an overdue batch in a burst.
            if tokio::time::Instant::now().saturating_duration_since(clock.deadline(frame.dts_ns))
                > Duration::from_millis(100)
            {
                clock.resume(frame.dts_ns);
            }
            let nals = nal_utils::parse_annex_b(&frame.data);
            let timestamp = clock.timestamp(frame.pts_ns);
            let packets = packetizer
                .packetize(&nals, timestamp, &prepared.codec)
                .into_iter()
                .map(Arc::new)
                .collect();
            consumer.on_frame(packets, frame.key).await;
            position.store(frame.pts_ns, Ordering::Release);
            last_sent = Some(frame.dts_ns);
            decode_position.store(frame.dts_ns, Ordering::Release);
            if *rate.borrow() > 1.0
                && let Some(active) = Session::active(&source)
                && !active.closed.load(Ordering::Acquire)
                && let Some((head, arrived)) = active.latest_decode_time()
            {
                if arrived.elapsed() < Duration::from_secs(1)
                    && head.abs_diff(frame.dts_ns) <= 100_000_000
                {
                    if !caught_up.swap(true, Ordering::AcqRel) {
                        catch_up_ready.notify_one();
                    }
                }
            }
        }
        loop {
            prepared.frames = (&mut next.0).await??;
            prepared.codec = prepared.disk.lock().codec.unwrap_or(prepared.codec);
            if !prepared.frames.is_empty() {
                if empty_since.take().is_some() {
                    clock.resume(prepared.frames.front().unwrap().dts_ns);
                }
                break;
            }
            let active = Session::active(&source).or_else(|| prepared.session.clone());
            if active
                .as_ref()
                .is_none_or(|s| s.closed.load(Ordering::Acquire))
            {
                notify(
                    &consumer,
                    &status,
                    PlaybackEvent::Ended {
                        timestamp: position.load(Ordering::Acquire) / 1_000_000,
                    },
                )
                .await;
                return Ok(());
            }
            prepared.session = active;
            if empty_since.is_none() {
                notify(
                    &consumer,
                    &status,
                    PlaybackEvent::Waiting {
                        timestamp: position.load(Ordering::Acquire) / 1_000_000,
                    },
                )
                .await;
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
    use gstreamer as gst;
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

#[cfg(test)]
mod speed_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn rates_scale_deadlines_and_rtp_but_preserve_composition_offsets() {
        let origin = 1_800_000_000_123_456_789;
        for rate in [0.25, 0.5, 1.0, 2.0, 4.0] {
            let clock = PlaybackClock::new(origin, rate);
            assert_eq!(
                clock.deadline(origin + 1_000_000_000) - clock.wall,
                Duration::from_secs_f64(1.0 / rate)
            );
            let delta = clock
                .timestamp(origin + 1_000_000_000)
                .wrapping_sub(clock.timestamp(origin));
            assert_eq!(delta, (90000.0 / rate) as u32);
            assert_eq!(
                clock
                    .timestamp(origin)
                    .wrapping_sub(clock.timestamp(origin - 100_000_000)),
                (9000.0 / rate) as u32
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn rate_changes_recompute_pending_deadline_without_timestamp_jump() {
        let mut clock = PlaybackClock::new(0, 0.5);
        tokio::time::advance(Duration::from_millis(500)).await;
        let before = clock.timestamp(250_000_000);
        clock.set_rate(4.0);
        assert_eq!(clock.timestamp(250_000_000), before);
        assert_eq!(
            clock.deadline(1_250_000_000) - tokio::time::Instant::now(),
            Duration::from_millis(250)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn one_x_retains_original_timestamp_mapping_including_wrap() {
        for origin in [0, 47_721_858_000_000, 1_800_000_000_123_456_789] {
            let clock = PlaybackClock::new(origin, 1.0);
            for offset in [0, 33_333_333, 66_666_666, 5_000_000_000] {
                assert_eq!(
                    clock.timestamp(origin + offset),
                    (((origin + offset) as u128 * 90000) / 1_000_000_000) as u32
                );
            }
        }
    }
}
