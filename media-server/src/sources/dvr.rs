use crate::app::VideoSourceId;
use crate::common::VideoCodec;
use crate::common::nal_utils;
use crate::common::rtp::RtpPacketizer;
use crate::common::traits::RtpConsumer;
use crate::domain::dvr::filesystem::{self, FindNextRecRes, RecordingMetadata};
use crate::domain::dvr::indexed_file::IndexedFile;
use anyhow::{Result, anyhow, bail};
use chrono::Duration;
use futures::StreamExt;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{self as gst_app, AppSinkCallbacks};
use media_server_api_models::UnixTimestamp;
use std::str::FromStr;
use std::sync::Arc;

struct GrowingSource {
    source: gst_app::AppSrc,
    reader: Arc<parking_lot::Mutex<IndexedFile>>,
}

struct CurrentPipelineState {
    pipeline: gst::Pipeline,
    growing_source: Option<GrowingSource>,
    appsink: gst_app::AppSink,
    watcher_handle: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
    #[allow(dead_code)]
    speed: f64,
    recording_metadata: RecordingMetadata,
    initial_time: UnixTimestamp,
    /// task that sends RTP packets to the consumer from gstreamer callbacks
    sender_handle: tokio::task::JoinHandle<()>,
}

impl Drop for CurrentPipelineState {
    fn drop(&mut self) {
        if let Some(handle) = self.watcher_handle.lock().take() {
            handle.abort();
        }
        self.sender_handle.abort();
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl CurrentPipelineState {
    async fn stop(&self) -> anyhow::Result<()> {
        if let Some(handle) = self.watcher_handle.lock().take() {
            handle.abort();
        }
        self.sender_handle.abort();
        // ideally you'd send EOS and wait for the bus to drain before setting Null
        // but sometimes the pipeline wasn't even running so you'd hang here forever
        let _ = self.pipeline.set_state(gst::State::Null);
        Ok(())
    }

    async fn set_state(&self, target: gst::State) -> Result<()> {
        let pipeline = self.pipeline.clone();
        tokio::task::spawn_blocking(move || {
            pipeline
                .set_state(target)
                .map_err(|e| anyhow!("failed to set DVR state: {e}"))?;
            let (result, current, pending) = pipeline.state(gst::ClockTime::from_seconds(5));
            result.map_err(|e| anyhow!("DVR state change failed: {e}"))?;
            if current != target || pending != gst::State::VoidPending {
                bail!("DVR pipeline did not reach {target:?} within five seconds");
            }
            Ok(())
        })
        .await?
    }

    async fn prepare(&self) -> Result<()> {
        if self.initial_time < self.recording_metadata.start_time {
            bail!("initial time is before recording start time");
        }
        // PAUSED prerolls without forwarding samples to the RTP consumer.
        self.set_state(gst::State::Paused).await?;
        self.seek_to_offset(self.initial_time)?;
        self.set_state(gst::State::Paused).await?;
        self.appsink
            .try_pull_preroll(gst::ClockTime::ZERO)
            .ok_or_else(|| anyhow!("DVR seek produced no video at the requested position"))?;
        Ok(())
    }

    fn seek_to_offset(&self, timestamp: UnixTimestamp) -> anyhow::Result<()> {
        let offset_ms = timestamp.saturating_sub(self.recording_metadata.start_time);
        let seek_pos = gst::ClockTime::from_mseconds(offset_ms);
        log::debug!("DVR seek to {} (offset_ms={})", timestamp, offset_ms);
        self.pipeline
            .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, seek_pos)
            .map_err(|e| anyhow!("Seek failed: {e}"))
    }

    async fn start(&self) -> Result<()> {
        if let Some(growing) = &self.growing_source {
            let mut reader = growing.reader.lock();
            reader.follow_growth()?;
            growing.source.set_size(i64::try_from(reader.size())?);
        }
        let bus = self.pipeline.bus().ok_or_else(|| anyhow!("no DVR bus"))?;
        let mut stream = bus.stream();
        self.set_state(gst::State::Playing).await?;
        *self.watcher_handle.lock() = Some(tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                match msg.view() {
                    gst::MessageView::Error(err) => {
                        log::error!("DVR playback failed: {} ({:?})", err.error(), err.debug());
                        break;
                    }
                    gst::MessageView::Eos(_) => {
                        log::info!("DVR playback reached end of available recording");
                        break;
                    }
                    _ => {}
                }
            }
        }));
        Ok(())
    }
}

#[allow(dead_code)]
enum DvrPlayerState {
    Scheduled(Duration),
    Playing(CurrentPipelineState),
}

pub struct DvrPlayer {
    #[allow(dead_code)]
    codec: VideoCodec,
    state: Arc<tokio::sync::Mutex<CurrentPipelineState>>,
    #[allow(dead_code)]
    source_id: VideoSourceId,
    #[allow(dead_code)]
    consumer: Arc<dyn RtpConsumer>,
    #[allow(dead_code)]
    reset_state_chan: (
        tokio::sync::mpsc::Sender<()>,
        tokio::sync::mpsc::Receiver<()>,
    ),
}

impl DvrPlayer {
    pub async fn new(
        source_id: VideoSourceId,
        initial_start_time: UnixTimestamp,
        // FIXME: this should be deduced automatically based on the file (we can also do some file name convention for this)
        codec: VideoCodec,
        consumer: Arc<dyn RtpConsumer>,
    ) -> Result<Self> {
        let source = source_id.clone();
        let output = consumer.clone();
        let initial_state = tokio::task::spawn_blocking(move || {
            Self::resolve_new_state(
                &source,
                filesystem::find_next_recording(&source, initial_start_time),
                initial_start_time,
                output,
                &codec,
            )
        })
        .await??;
        let (reset_sender, reset_receiver) = tokio::sync::mpsc::channel(1);

        Ok(Self {
            codec,
            source_id,
            state: Arc::new(tokio::sync::Mutex::new(initial_state)),
            consumer,
            reset_state_chan: (reset_sender, reset_receiver),
        })
    }
    pub async fn current_timestamp(&self) -> UnixTimestamp {
        let state_guard = self.state.lock().await;
        let start_time = state_guard.recording_metadata.start_time;
        state_guard
            .pipeline
            .query_position::<gst::ClockTime>()
            .map(|pos| start_time.saturating_add(pos.mseconds()))
            .unwrap_or(start_time)
    }

    pub async fn current_time_ms(&self) -> Option<u64> {
        let state_guard = self.state.lock().await;
        state_guard
            .pipeline
            .query_position::<gst::ClockTime>()
            .map(|pos| pos.mseconds())
    }

    pub fn speed(&self) -> f64 {
        // Speed is stored in the state, but we'd need async to access it
        // For now, return 1.0 as default
        1.0
    }

    pub fn set_speed(&self, _speed: f64) {
        // Speed control requires seeking with a rate parameter
        // This is a placeholder - full implementation would need async
        log::warn!("set_speed not fully implemented yet");
    }

    fn resolve_new_state(
        source_id: &VideoSourceId,
        res: FindNextRecRes,
        initial_start_time: UnixTimestamp,
        consumer: Arc<dyn RtpConsumer>,
        codec: &VideoCodec,
    ) -> anyhow::Result<CurrentPipelineState> {
        match res {
            Some((recording, None)) => {
                create_pipeline(&recording, initial_start_time, codec, consumer)
            }
            // FIXME: maybe we should wait until the recording is available?
            _ => bail!("No recording found for source ID {}", source_id),
        }
    }

    pub async fn play(&self) -> anyhow::Result<()> {
        let state = self.state.lock().await;
        state.prepare().await?;
        self.consumer.on_source_switch();
        state.start().await
    }

    pub async fn terminate(&self) -> Result<()> {
        let state_guard = self.state.lock().await;
        state_guard.stop().await
    }

    pub async fn recording_metadata(&self) -> RecordingMetadata {
        self.state.lock().await.recording_metadata.clone()
    }

    pub async fn seek_to_timestamp(&self, timestamp: u64, _speed: f64) -> Result<(), SeekError> {
        let mut state_guard = self.state.lock().await;
        let recording = state_guard.recording_metadata.clone();
        let codec = self.codec;
        let consumer = self.consumer.clone();
        // Refresh the snapshot on every seek, including fragments appended since
        // this player was opened. Prepare first so a failed seek preserves playback.
        let mut replacement = tokio::task::spawn_blocking(move || {
            create_pipeline(&recording, timestamp, &codec, consumer)
        })
        .await
        .map_err(|e| SeekError::GstError(e.into()))?
        .map_err(SeekError::GstError)?;
        replacement.speed = state_guard.speed;
        replacement.prepare().await.map_err(SeekError::GstError)?;
        state_guard.stop().await.map_err(SeekError::GstError)?;
        self.consumer.on_source_switch();
        replacement.start().await.map_err(SeekError::GstError)?;
        *state_guard = replacement;
        Ok(())
    }
}

fn create_pipeline(
    recording: &RecordingMetadata,
    timestamp: UnixTimestamp,
    src_codec: &VideoCodec,
    consumer: Arc<dyn RtpConsumer>,
) -> Result<CurrentPipelineState> {
    let path_str = recording.path.to_string_lossy().to_string();
    log::info!("Opening DVR file: {}", path_str);

    let pipeline = gst::Pipeline::new();
    let growing_source = if recording.end_time.is_none() {
        Some(indexed_source(
            &recording.path,
            timestamp.saturating_sub(recording.start_time),
        )?)
    } else {
        None
    };
    let src = if let Some(growing) = &growing_source {
        growing.source.clone().upcast::<gst::Element>()
    } else {
        gst::ElementFactory::make("filesrc")
            .property("location", &path_str)
            .build()?
    };
    let demux = gst::ElementFactory::make("qtdemux")
        .build()
        .map_err(|_| anyhow::anyhow!("Failed to create qtdemux"))?;
    let queue = gst::ElementFactory::make("queue")
        .build()
        .map_err(|_| anyhow::anyhow!("Failed to create queue"))?;
    let parser = match src_codec {
        VideoCodec::H264 => gst::ElementFactory::make("h264parse")
            .build()
            .map_err(|_| anyhow::anyhow!("Failed to create h264parse"))?,
        VideoCodec::H265 => gst::ElementFactory::make("h265parse")
            .build()
            .map_err(|_| anyhow::anyhow!("Failed to create h265parse"))?,
    };

    // Configure parser to insert SPS/PPS before every IDR frame
    parser.set_property_from_str("config-interval", "-1");

    // Create a capsfilter to force byte-stream (Annex-B) output format
    // This is necessary because h264parse by default preserves the input format (AVCC from MP4)
    let capsfilter = gst::ElementFactory::make("capsfilter")
        .build()
        .map_err(|_| anyhow::anyhow!("Failed to create capsfilter"))?;

    let caps_str = match src_codec {
        VideoCodec::H264 => "video/x-h264,stream-format=byte-stream,alignment=au",
        VideoCodec::H265 => "video/x-h265,stream-format=byte-stream,alignment=au",
    };
    let caps = gst::Caps::from_str(caps_str)
        .map_err(|e| anyhow::anyhow!("Failed to create caps: {}", e))?;
    capsfilter.set_property("caps", &caps);

    let appsink = gst::ElementFactory::make("appsink")
        .build()
        .map_err(|_| anyhow::anyhow!("Failed to create appsink"))?;
    let appsink = appsink
        .dynamic_cast::<gst_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("Failed to cast appsink"))?;
    appsink.set_property("emit-signals", false);
    appsink.set_property("sync", true); // Enable sync for proper timing

    // Note: No RTP payloader - we output raw H264/H265 and packetize ourselves
    pipeline.add_many([
        &src,
        &demux,
        &queue,
        &parser,
        &capsfilter,
        appsink.upcast_ref(),
    ])?;

    src.link(&demux)
        .map_err(|_| anyhow::anyhow!("Failed to link filesrc to qtdemux"))?;
    queue
        .link(&parser)
        .map_err(|_| anyhow::anyhow!("Failed to link queue to parser"))?;
    parser
        .link(&capsfilter)
        .map_err(|_| anyhow::anyhow!("Failed to link parser to capsfilter"))?;
    capsfilter
        .link(&appsink)
        .map_err(|_| anyhow::anyhow!("Failed to link capsfilter to appsink"))?;

    let queue_clone = queue.clone();
    demux.connect_pad_added(move |_demux, src_pad| {
        let sink_pad = match queue_clone.static_pad("sink") {
            Some(pad) => pad,
            None => return,
        };
        if sink_pad.is_linked() {
            return;
        }
        if let Some(caps) = src_pad.current_caps()
            && let Some(structure) = caps.structure(0)
            && !structure.name().starts_with("video/")
        {
            return;
        }
        let _ = src_pad.link(&sink_pad);
    });

    // Channel now carries (raw_data, pts_in_clock_time_nanoseconds)
    let (packet_tx, mut packet_rx) = tokio::sync::mpsc::channel::<(Vec<u8>, Option<u64>)>(30);

    appsink.set_callbacks(
        AppSinkCallbacks::builder()
            .new_sample(move |appsink| {
                let sample = appsink
                    .pull_sample()
                    .map_err(|_| gst::FlowError::CustomError)?;
                if let Some(buffer) = sample.buffer_owned() {
                    let mut data = vec![0u8; buffer.size()];
                    buffer
                        .copy_to_slice(0, &mut data)
                        .map_err(|_| gst::FlowError::CustomError)?;
                    // Get PTS from the buffer (in nanoseconds)
                    let pts = buffer.pts().map(|t| t.nseconds());
                    if let Err(e) = packet_tx.try_send((data, pts)) {
                        log::warn!("DVR packet channel full or closed: {}", e);
                    }
                }
                Ok(gstreamer::FlowSuccess::Ok)
            })
            .build(),
    );

    // Create RTP packetizer for DVR playback
    // Use random SSRC and payload type 96 (dynamic, same as live stream)
    let rtp_packetizer = RtpPacketizer::new(rand::random::<u32>(), 96);
    let codec_clone = *src_codec;

    let sender_handle = tokio::spawn(async move {
        let mut frame_count = 0u64;
        while let Some((raw_data, pts)) = packet_rx.recv().await {
            // Convert PTS from nanoseconds to RTP timestamp (90kHz clock)
            // pts is in nanoseconds, RTP clock is 90000 Hz
            // rtp_ts = pts_ns * 90000 / 1_000_000_000 = pts_ns / 11111.111...
            let rtp_timestamp = pts
                .map(|p| (p as u128 * 90000 / 1_000_000_000) as u32)
                .unwrap_or(0);

            // Parse NAL units from the raw data
            // h264parse outputs byte-stream (Annex-B) format by default
            // But let's check if it's actually Annex-B or AVCC format
            let nal_units = if nal_utils::is_annex_b(&raw_data) {
                nal_utils::parse_annex_b(&raw_data)
            } else {
                // Try AVCC with 4-byte length prefix (common for MP4)
                nal_utils::parse_avcc(&raw_data, &4)
            };

            if nal_units.is_empty() {
                // Log first bytes to understand the format
                let first_bytes: Vec<String> = raw_data
                    .iter()
                    .take(16)
                    .map(|b| format!("{:02x}", b))
                    .collect();
                log::warn!(
                    "DVR: empty NAL units from {} bytes, first_bytes=[{}], is_annex_b={}",
                    raw_data.len(),
                    first_bytes.join(" "),
                    nal_utils::is_annex_b(&raw_data)
                );
                continue;
            }

            frame_count += 1;
            if frame_count <= 5 || frame_count.is_multiple_of(100) {
                log::trace!(
                    "DVR frame {}: raw_size={}, pts={:?}, rtp_ts={}, nal_count={}",
                    frame_count,
                    raw_data.len(),
                    pts,
                    rtp_timestamp,
                    nal_units.len()
                );
            }

            // Packetize NAL units into RTP packets
            let rtp_packets = rtp_packetizer.packetize(&nal_units, rtp_timestamp, &codec_clone);

            if frame_count <= 5 {
                log::trace!(
                    "DVR frame {}: generated {} RTP packets",
                    frame_count,
                    rtp_packets.len()
                );
            }

            // Send each RTP packet to the consumer
            for rtp_packet in rtp_packets {
                let packet_arc = Arc::new(rtp_packet);
                consumer.on_new_packet(packet_arc).await;
            }
        }
    });
    Ok(CurrentPipelineState {
        pipeline,
        appsink,
        sender_handle,
        growing_source,
        watcher_handle: parking_lot::Mutex::new(None),
        speed: 1.0,
        recording_metadata: recording.clone(),
        initial_time: timestamp,
    })
}

fn indexed_source(path: &std::path::Path, offset_ms: u64) -> Result<GrowingSource> {
    let file = IndexedFile::open(path, offset_ms)?;
    let size = i64::try_from(file.size())?;
    let file = Arc::new(parking_lot::Mutex::new(file));
    let seek_file = file.clone();
    let read_file = file.clone();
    let source = gst_app::AppSrc::builder()
        .stream_type(gst_app::AppStreamType::RandomAccess)
        .format(gst::Format::Bytes)
        .size(size)
        .build();
    source.set_callbacks(
        gst_app::AppSrcCallbacks::builder()
            .seek_data(move |_, offset| seek_file.lock().seek(offset).is_ok())
            .need_data(move |source, length| {
                let result = {
                    let mut reader = read_file.lock();
                    let result = reader.read(length as usize);
                    if let Ok(size) = i64::try_from(reader.size()) {
                        source.set_size(size);
                    }
                    result
                };
                match result {
                    Ok(data) if data.is_empty() => {
                        let _ = source.end_of_stream();
                    }
                    Ok(data) => {
                        let _ = source.push_buffer(gst::Buffer::from_mut_slice(data));
                    }
                    Err(err) => gst::element_error!(
                        source,
                        gst::ResourceError::Read,
                        ("Could not read DVR snapshot"),
                        ["{err:#}"]
                    ),
                }
            })
            .build(),
    );
    Ok(GrowingSource {
        source,
        reader: file,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum SeekError {
    #[error("seek before start")]
    SeekBeforeStart,
    #[error("seek after end")]
    SeekAfterEnd,
    #[error(transparent)]
    GstError(anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::rtp::RtpPacket;
    use async_trait::async_trait;
    use media_server_api_models::ClientSessionId;
    use std::time::{Duration as StdDuration, Instant};

    struct Capture {
        id: ClientSessionId,
        tx: tokio::sync::mpsc::UnboundedSender<u32>,
    }

    #[async_trait]
    impl RtpConsumer for Capture {
        fn id(&self) -> &ClientSessionId {
            &self.id
        }
        async fn on_new_packet(&self, packet: Arc<RtpPacket>) {
            if packet.header.marker {
                let _ = self.tx.send(packet.header.timestamp);
            }
        }
    }

    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new(bytes: &[u8]) -> Self {
            let path =
                std::env::temp_dir().join(format!("dvr-rewind-{}.mp4", uuid::Uuid::new_v4()));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn fragment_prefix(data: &[u8], count: usize) -> usize {
        let mut pos = 0;
        let mut fragments = 0;
        while pos + 8 <= data.len() {
            let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            if &data[pos + 4..pos + 8] == b"mdat" {
                fragments += 1;
                if fragments == count {
                    return pos + size;
                }
            }
            pos += size;
        }
        panic!("fixture has fewer than {count} fragments");
    }

    async fn player(
        path: std::path::PathBuf,
        offset: u64,
        codec: VideoCodec,
        active: bool,
    ) -> (DvrPlayer, tokio::sync::mpsc::UnboundedReceiver<u32>) {
        gst::init().unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let consumer = Arc::new(Capture {
            id: uuid::Uuid::new_v4(),
            tx,
        });
        let recording = RecordingMetadata {
            path,
            start_time: 0,
            end_time: if active { None } else { Some(12000) },
        };
        let source = "test".to_string();
        let output = consumer.clone();
        let initial = tokio::task::spawn_blocking(move || {
            DvrPlayer::resolve_new_state(&source, Some((recording, None)), offset, output, &codec)
        })
        .await
        .unwrap()
        .unwrap();
        let (reset_tx, reset_rx) = tokio::sync::mpsc::channel(1);
        (
            DvrPlayer {
                codec,
                state: Arc::new(tokio::sync::Mutex::new(initial)),
                source_id: "test".into(),
                consumer,
                reset_state_chan: (reset_tx, reset_rx),
            },
            rx,
        )
    }

    async fn assert_playback(rx: &mut tokio::sync::mpsc::UnboundedReceiver<u32>, offset: u64) {
        let expected = (offset as u128 * 90) as u32;
        let first = tokio::time::timeout(StdDuration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let delta = first.wrapping_sub(expected) as i32;
        assert!(
            delta.unsigned_abs() <= 90000,
            "first frame {first} is not near requested RTP timestamp {expected}"
        );
        let mut last = first;
        for _ in 0..60 {
            last = tokio::time::timeout(StdDuration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
        }
        assert!(
            last.wrapping_sub(first) > 45000,
            "playback did not progress"
        );
    }

    #[tokio::test]
    async fn active_recording_seek_skips_preroll_and_refreshes_snapshot() {
        let data = include_bytes!("../../tests/fixtures/dvr-fragmented.mp4");
        let prefix = fragment_prefix(data, 7);
        let fixture = Fixture::new(&data[..prefix]);
        let (player, mut rx) = player(fixture.0.clone(), 4000, VideoCodec::H264, true).await;
        player.play().await.unwrap();
        assert_playback(&mut rx, 4000).await;
        // Complete more fragments after the first snapshot was opened.
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&fixture.0)
            .unwrap()
            .write_all(&data[prefix..])
            .unwrap();
        player.seek_to_timestamp(9000, 1.0).await.unwrap();
        // Packets already sent before the seek remain in this test receiver.
        while rx.try_recv().is_ok() {}
        assert_playback(&mut rx, 9000).await;
        assert!(player.current_time_ms().await.unwrap() >= 9000);
        player.terminate().await.unwrap();
    }

    #[tokio::test]
    async fn active_playback_follows_fragments_appended_after_seek() {
        use std::io::Write;
        let data = include_bytes!("../../tests/fixtures/dvr-fragmented.mp4");
        let prefix = fragment_prefix(data, 7);
        let fixture = Fixture::new(&data[..prefix]);
        let (player, mut rx) = player(fixture.0.clone(), 4000, VideoCodec::H264, true).await;
        player.play().await.unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&fixture.0)
            .unwrap()
            .write_all(&data[prefix..])
            .unwrap();
        assert_playback(&mut rx, 4000).await;
        // Advance beyond the seven seconds present when the seek began.
        loop {
            let timestamp = tokio::time::timeout(StdDuration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            if timestamp >= 9000 * 90 {
                break;
            }
        }
        player.terminate().await.unwrap();
    }

    #[tokio::test]
    async fn finalized_recording_seeks_before_sending_video() {
        let fixture = Fixture::new(include_bytes!("../../tests/fixtures/dvr-fragmented.mp4"));
        let (player, mut rx) = player(fixture.0.clone(), 7000, VideoCodec::H264, false).await;
        player.play().await.unwrap();
        assert_playback(&mut rx, 7000).await;
        player.terminate().await.unwrap();
    }

    #[tokio::test]
    async fn seek_beyond_recorded_data_fails_without_sending_video() {
        let fixture = Fixture::new(include_bytes!("../../tests/fixtures/dvr-fragmented.mp4"));
        let (player, mut rx) = player(fixture.0.clone(), 60000, VideoCodec::H264, true).await;
        assert!(player.play().await.is_err());
        assert!(rx.try_recv().is_err());
        player.terminate().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "set DVR_TEST_PATH and DVR_TEST_OFFSET_MS to validate an existing H.265 recording"]
    async fn existing_active_recording_rewind() {
        let path = std::env::var_os("DVR_TEST_PATH").expect("DVR_TEST_PATH");
        let offset: u64 = std::env::var("DVR_TEST_OFFSET_MS")
            .expect("DVR_TEST_OFFSET_MS")
            .parse()
            .unwrap();
        let start = Instant::now();
        let (player, mut rx) = player(path.into(), offset, VideoCodec::H265, true).await;
        player.play().await.unwrap();
        eprintln!(
            "DVR ready in {:?}, position {:?}",
            start.elapsed(),
            player.current_time_ms().await
        );
        assert_playback(&mut rx, offset).await;
        player.terminate().await.unwrap();
    }
}
