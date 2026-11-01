use crate::app::VideoSourceId;
use crate::common::VideoCodec;
use crate::common::nal_utils::{self, FramingFormat};
use crate::common::traits::FfmpegConsumer;
use crate::domain::dvr::segments::{self, Frame, Segment, Session};
use anyhow::{Result, bail};
use axum::async_trait;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use media_server_api_models::UnixTimestamp;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;

struct RecordingState {
    session: Arc<Session>,
    timebase: Option<crate::common::TimeBase>,
    timestamp_origin: Option<i64>,
    packet_timing: Option<(u64, u64)>,
    framing_format: FramingFormat,
    file_path: PathBuf,
    start_time: UnixTimestamp,
    pipeline: gst::Pipeline,
    appsrc: gst_app::AppSrc,
    codec: VideoCodec,
    waiting_for_keyframe: bool,
    header_buffer: Vec<Vec<u8>>,
    stream_started: bool,
}

impl Drop for RecordingState {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        self.session.close();
    }
}

impl RecordingState {
    fn new(
        session: Arc<Session>,
        codec: &VideoCodec,
        start_time: UnixTimestamp,
        framing_format: FramingFormat,
    ) -> Result<Self> {
        let pipeline = gst::Pipeline::new();

        let appsrc = gst::ElementFactory::make("appsrc")
            .build()
            .map_err(|_| anyhow::anyhow!("Failed to create appsrc"))?;

        appsrc.set_property("stream-type", gst_app::AppStreamType::Stream);
        appsrc.set_property("format", gst::Format::Time);
        appsrc.set_property("is-live", true);
        appsrc.set_property("do-timestamp", false);

        let caps = match codec {
            VideoCodec::H264 => gst::Caps::builder("video/x-h264")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
            VideoCodec::H265 => gst::Caps::builder("video/x-h265")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        };
        appsrc.set_property("caps", &caps);

        let parser = match codec {
            VideoCodec::H264 => gst::ElementFactory::make("h264parse")
                .build()
                .map_err(|_| anyhow::anyhow!("Failed to create h264parse"))?,
            VideoCodec::H265 => gst::ElementFactory::make("h265parse")
                .build()
                .map_err(|_| anyhow::anyhow!("Failed to create h265parse"))?,
        };
        parser.set_property_from_str("config-interval", "-1");

        let path = session.directory.join("%010d.mp4");
        let sink = gst::ElementFactory::make("splitmuxsink")
            .property("location", path.to_string_lossy().as_ref())
            .property("max-size-time", segments::SEGMENT_NS)
            .property("max-size-bytes", 0u64)
            .property("async-finalize", true)
            .property(
                "muxer-properties",
                gst::Structure::builder("properties")
                    .field("trak-timescale", 1_000_000u32)
                    .build(),
            )
            .build()?;
        let pending = Arc::new(Mutex::new(
            std::collections::HashMap::<String, Segment>::new(),
        ));
        let starts = pending.clone();
        let catalog = session.clone();
        sink.connect("format-location-full", false, move |values| {
            let id = values[1].get::<u32>().expect("fragment ID");
            let sample = values[2]
                .get::<gst::Sample>()
                .expect("first fragment sample");
            let first_ns = sample
                .buffer()
                .and_then(|b| b.pts())
                .map(|t| t.nseconds())
                .unwrap_or(0);
            let location = catalog.directory.join(format!("{id:010}.mp4"));
            let location_string = location.to_string_lossy().into_owned();
            starts.lock().insert(
                location_string.clone(),
                Segment {
                    origin_ms: catalog.origin_ms,
                    first_ns,
                    end_ns: first_ns,
                    codec: match catalog.codec {
                        VideoCodec::H264 => "h264",
                        VideoCodec::H265 => "h265",
                    }
                    .into(),
                    path: location,
                },
            );
            Some(location_string.to_value())
        });
        pipeline
            .bus()
            .ok_or_else(|| anyhow::anyhow!("No recorder bus"))?
            .set_sync_handler(move |_, message| {
                if let Some(structure) = message.structure()
                    && structure.name() == "splitmuxsink-fragment-closed"
                    && let Ok(location) = structure.get::<String>("location")
                    && let Some(mut segment) = pending.lock().remove(&location)
                {
                    segment.end_ns = structure
                        .get::<gst::ClockTime>("running-time")
                        .map(|t| t.nseconds())
                        .unwrap_or(segment.first_ns);
                    if let Err(error) = segment.publish() {
                        log::error!("Failed to publish DVR segment {location}: {error:#}");
                    }
                }
                gst::BusSyncReply::Pass
            });
        pipeline.add_many([&appsrc, &parser, &sink])?;
        appsrc.link(&parser)?;
        parser.link(&sink)?;

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| anyhow::anyhow!("Failed to set pipeline to playing state"))?;

        let appsrc_typed = appsrc
            .dynamic_cast::<gst_app::AppSrc>()
            .map_err(|_| anyhow::anyhow!("Failed to cast to AppSrc"))?;

        Ok(Self {
            pipeline,
            file_path: session.directory.clone(),
            session,
            timebase: None,
            timestamp_origin: None,
            packet_timing: None,
            codec: *codec,
            waiting_for_keyframe: true,
            start_time,
            appsrc: appsrc_typed,
            header_buffer: Vec::new(),
            stream_started: false,
            framing_format,
        })
    }

    /// Parse NAL units from packet data, auto-detecting the framing format.
    /// FFmpeg RTSP often sends Annex-B even when extradata parsing suggested AVCC.
    fn parse_nal_units(&self, data: &[u8]) -> Vec<Vec<u8>> {
        let is_annexb = nal_utils::is_annex_b(data);
        let nals = if is_annexb {
            nal_utils::parse_annex_b(data)
        } else {
            nal_utils::parse_nal_units_with_length(data, &self.framing_format)
        };

        if nals.is_empty() && !data.is_empty() {
            log::warn!(
                "DVR: Failed to parse NAL units from {} bytes, is_annexb={}, first_bytes={:02x?}",
                data.len(),
                is_annexb,
                &data[..std::cmp::min(16, data.len())]
            );
        }

        nals
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        let result = (|| -> Result<()> {
            self.appsrc.end_of_stream()?;
            let bus = self
                .pipeline
                .bus()
                .ok_or_else(|| anyhow::anyhow!("Pipeline has no bus"))?;
            let message = bus
                .timed_pop_filtered(
                    gst::ClockTime::from_seconds(5),
                    &[gst::MessageType::Eos, gst::MessageType::Error],
                )
                .ok_or_else(|| anyhow::anyhow!("DVR finalization timed out"))?;
            if let gst::MessageView::Error(error) = message.view() {
                bail!("DVR finalization failed: {}", error.error());
            }
            Ok(())
        })();
        let stopped = self.pipeline.set_state(gst::State::Null);
        self.session.close();
        result?;
        stopped?;
        self.waiting_for_keyframe = true;
        self.header_buffer.clear();
        self.stream_started = false;
        Ok(())
    }

    fn push_buffer_to_appsrc(&self, data: &[u8], is_keyframe: bool) -> Result<()> {
        // Use wall-clock elapsed time as PTS/DTS so that playback duration
        // matches real time even when camera frames arrive in bursts.
        let elapsed_ns = self.session.started.elapsed().as_nanos() as u64;

        if let Some((pts, dts)) = self.packet_timing {
            self.push_timed(data, is_keyframe, pts, dts)
        } else {
            self.push_at(data, is_keyframe, elapsed_ns)
        }
    }

    fn push_at(&self, data: &[u8], is_keyframe: bool, elapsed_ns: u64) -> Result<()> {
        self.push_timed(data, is_keyframe, elapsed_ns, elapsed_ns)
    }

    fn push_timed(&self, data: &[u8], is_keyframe: bool, pts_ns: u64, dts_ns: u64) -> Result<()> {
        // Use the same microsecond precision in MP4 and memory, allowing exact
        // overlap matching without a timestamp tolerance at handoff.
        let pts_ns = pts_ns / 1_000 * 1_000;
        let dts_ns = dts_ns / 1_000 * 1_000;
        let mut buffer = gst::Buffer::from_slice(data.to_vec());
        {
            let buffer_ref = buffer.get_mut().unwrap();
            buffer_ref.set_pts(gst::ClockTime::from_nseconds(pts_ns));
            buffer_ref.set_dts(gst::ClockTime::from_nseconds(dts_ns));

            if !is_keyframe {
                buffer_ref.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }

        self.session.push(Frame {
            pts_ns,
            dts_ns,
            key: is_keyframe,
            data: Arc::new(data.to_vec()),
        });
        match self.appsrc.push_buffer(buffer) {
            Ok(_) => Ok(()),
            Err(gst::FlowError::Flushing) => Ok(()),
            Err(e) => bail!("Failed to push buffer: {:?}", e),
        }
    }

    fn write_nal_data(&mut self, nal_units: &[Vec<u8>], is_key_frame: bool) -> Result<()> {
        if nal_units.is_empty() {
            return Ok(());
        }

        let mut header_nals = Vec::new();

        for nal in nal_units {
            if nal.is_empty() {
                continue;
            }
            let is_header = nal_utils::is_parameter_set(nal, &self.codec);

            if is_header {
                header_nals.push(nal.clone());
                continue;
            }
        }

        if !header_nals.is_empty() {
            for header in header_nals {
                let nal_type = |nal: &[u8]| match self.codec {
                    VideoCodec::H264 => nal[0] & 0x1f,
                    VideoCodec::H265 => (nal[0] >> 1) & 0x3f,
                };
                self.header_buffer
                    .retain(|old| nal_type(old) != nal_type(&header));
                self.header_buffer.push(header);
            }
            log::debug!(
                "Buffered headers (total headers: {})",
                self.header_buffer.len()
            );
        }

        if self.waiting_for_keyframe {
            if !is_key_frame {
                return Ok(());
            }

            log::info!(
                "First keyframe detected, starting recording with {} headers",
                self.header_buffer.len()
            );

            if !self.stream_started {
                let stream_id = format!("stream_{:?}", self.file_path);
                if let Some(src_pad) = self.appsrc.static_pad("src") {
                    let _ = src_pad.push_event(gst::event::StreamStart::new(&stream_id));
                }
                self.stream_started = true;
            }

            let combined_bytes = if self.header_buffer.is_empty() {
                nal_utils::build_annex_b(nal_units)
            } else {
                let mut combined = Vec::new();
                combined.extend_from_slice(&nal_utils::build_annex_b(&self.header_buffer));
                combined.extend_from_slice(&nal_utils::build_annex_b(nal_units));
                combined
            };
            self.push_buffer_to_appsrc(&combined_bytes, true)?;

            self.waiting_for_keyframe = false;
            return Ok(());
        }

        let mut packet_bytes = Vec::new();
        if is_key_frame {
            packet_bytes.extend_from_slice(&nal_utils::build_annex_b(&self.header_buffer));
        }
        packet_bytes.extend_from_slice(&nal_utils::build_annex_b(nal_units));
        self.push_buffer_to_appsrc(&packet_bytes, is_key_frame)?;

        Ok(())
    }
}

pub struct DvrRecorder {
    stream_id: VideoSourceId,
    recording_state: Mutex<Option<RecordingState>>,
}

impl DvrRecorder {
    pub fn new(stream_id: &VideoSourceId) -> Result<Self> {
        Ok(Self {
            stream_id: stream_id.clone(),
            recording_state: Mutex::new(None),
        })
    }

    fn initialize_imp(&self, codec: VideoCodec, extradata: Option<&Vec<u8>>) -> Result<PathBuf> {
        let mut state_guard = self.recording_state.lock();
        if let Some(mut state) = state_guard.take() {
            log::info!("already initialized; restarting...");
            if let Err(err) = state.stop() {
                log::error!("Failed to stop recording: {}", err);
            }
        }

        // Determine framing format from extradata and extract SPS/PPS
        let (framing_format, extradata_nals) = if let Some(extradata) = extradata {
            log::info!(
                "DVR extradata: size={}, first_bytes={:02x?}",
                extradata.len(),
                &extradata[..std::cmp::min(32, extradata.len())]
            );
            let parsed = match codec {
                VideoCodec::H264 => nal_utils::parse_h264_extradata(extradata),
                VideoCodec::H265 => nal_utils::parse_h265_extradata(extradata),
            };
            match parsed {
                Some(p) => {
                    log::info!(
                        "DVR parsed extradata: framing={:?}, nals={}",
                        p.framing_format,
                        p.nals.len()
                    );
                    for (i, nal) in p.nals.iter().enumerate() {
                        log::info!(
                            "  extradata NAL[{}]: type_byte=0x{:02x}, size={}",
                            i,
                            nal.first().copied().unwrap_or(0),
                            nal.len()
                        );
                    }
                    (p.framing_format, p.nals)
                }
                None => {
                    log::warn!("DVR: failed to parse extradata");
                    (FramingFormat::Avcc { length_size: 4 }, Vec::new())
                }
            }
        } else {
            log::warn!("DVR: no extradata provided");
            // Default to AVCC with 4-byte length if no extradata
            (FramingFormat::Avcc { length_size: 4 }, Vec::new())
        };

        log::info!("DVR recorder using framing format: {:?}", framing_format,);

        let start_time = crate::utils::get_current_unix_timestamp();
        let session = Session::new(&self.stream_id, start_time, codec)?;
        let path = session.directory.clone();
        let mut new_state = RecordingState::new(session, &codec, start_time, framing_format)?;

        // Pre-populate header buffer with SPS/PPS from extradata
        if !extradata_nals.is_empty() {
            log::info!(
                "DVR: pre-populating header buffer with {} NALs from extradata",
                extradata_nals.len()
            );
            new_state.header_buffer = extradata_nals;
        }

        *state_guard = Some(new_state);

        log::info!(
            "Recording started for stream {} with codec {:?}: {:?}",
            self.stream_id,
            codec,
            path
        );

        Ok(path)
    }

    pub fn finalize_recording(&self) -> Result<Option<PathBuf>> {
        let mut state_guard = self.recording_state.lock();

        if let Some(mut state) = state_guard.take() {
            let path = state.file_path.clone();
            state.stop()?;
            return Ok(Some(path));
        }

        Ok(None)
    }

    pub fn current_path(&self) -> Option<PathBuf> {
        self.recording_state
            .lock()
            .as_ref()
            .map(|s| s.file_path.clone())
    }

    pub fn stream_id(&self) -> &VideoSourceId {
        &self.stream_id
    }

    pub fn start_time(&self) -> Option<UnixTimestamp> {
        self.recording_state.lock().as_ref().map(|s| s.start_time)
    }
}

impl Drop for DvrRecorder {
    fn drop(&mut self) {
        if let Err(e) = self.finalize_recording() {
            log::error!(
                "Error finalizing recording during drop for stream {}: {}",
                self.stream_id,
                e
            );
        }
    }
}

#[async_trait]
impl FfmpegConsumer for DvrRecorder {
    fn initialize(&self, metadata: &crate::common::FFmpegVideoMetadata) -> anyhow::Result<()> {
        self.initialize_imp(metadata.codec, metadata.extradata.as_ref())?;
        if let Some(state) = self.recording_state.lock().as_mut() {
            state.timebase = Some(metadata.timebase.clone());
        }
        Ok(())
    }

    async fn on_new_packet(&self, packet: Arc<ffmpeg::Packet>) -> Result<()> {
        let mut state_guard = self.recording_state.lock();

        if let Some(state) = state_guard.as_mut()
            && let Some(data) = packet.data()
        {
            state.packet_timing = None;
            if let (Some(timebase), Some(pts), Some(dts)) =
                (&state.timebase, packet.pts(), packet.dts())
                && timebase.is_valid()
            {
                let origin = *state.timestamp_origin.get_or_insert(dts.min(pts));
                let convert = |value: i64| -> Result<u64> {
                    let ns =
                        (value as i128 - origin as i128) * timebase.num as i128 * 1_000_000_000
                            / timebase.den as i128;
                    Ok(u64::try_from(ns)?)
                };
                state.packet_timing = Some((convert(pts)?, convert(dts)?));
            }
            let is_key = packet.is_key();
            let nal_units = state.parse_nal_units(data);
            state.write_nal_data(&nal_units, is_key)?;
        }

        Ok(())
    }

    async fn finalize(&self) -> Result<()> {
        self.finalize_recording()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        common::{rtp::RtpPacket, traits::RtpConsumer},
        sources::segmented_dvr::{SegmentedPlayer, read_segment},
    };
    use media_server_api_models::ClientSessionId;
    use std::time::{Duration, Instant};

    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    struct Capture {
        id: ClientSessionId,
        tx: tokio::sync::mpsc::UnboundedSender<(u16, u32)>,
        previous: Mutex<Option<(u16, u32)>>,
    }
    #[async_trait]
    impl RtpConsumer for Capture {
        fn id(&self) -> &ClientSessionId {
            &self.id
        }
        fn on_source_switch(&self) {
            *self.previous.lock() = None;
        }
        async fn on_new_packet(&self, packet: Arc<RtpPacket>) {
            let mut previous = self.previous.lock();
            if let Some((sequence, ssrc)) = *previous {
                assert_eq!(packet.header.seq, sequence.wrapping_add(1));
                assert_eq!(packet.header.ssrc, ssrc);
            }
            *previous = Some((packet.header.seq, packet.header.ssrc));
            if packet.header.marker {
                let _ = self.tx.send((packet.header.seq, packet.header.timestamp));
            }
        }
    }
    fn fixture_frames(codec: VideoCodec) -> Vec<Frame> {
        gst::init().unwrap();
        read_segment(Segment {
            origin_ms: 0,
            first_ns: 0,
            end_ns: 12_000_000_000,
            codec: match codec {
                VideoCodec::H264 => "h264",
                VideoCodec::H265 => "h265",
            }
            .into(),
            path: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(match codec {
                VideoCodec::H264 => "tests/fixtures/dvr-fragmented.mp4",
                VideoCodec::H265 => "tests/fixtures/dvr-fragmented-h265.mp4",
            }),
        })
        .unwrap()
    }
    fn recording(
        seconds: u64,
        codec: VideoCodec,
    ) -> (
        VideoSourceId,
        Arc<Session>,
        RecordingState,
        Cleanup,
        Vec<Frame>,
    ) {
        gst::init().unwrap();
        let source = format!("segmented-test-{}", uuid::Uuid::new_v4());
        let session = Session::new(&source, 1_700_000_000_000, codec).unwrap();
        let cleanup = Cleanup(session.directory.parent().unwrap().to_path_buf());
        let state = RecordingState::new(
            session.clone(),
            &codec,
            session.origin_ms,
            FramingFormat::AnnexB,
        )
        .unwrap();
        let fixture = fixture_frames(codec);
        let mut input = Vec::new();
        for i in 0..seconds * 30 {
            let frame = &fixture[i as usize % fixture.len()];
            // Nonzero and sub-millisecond start exercises exact timestamp mapping.
            let frame = Frame {
                pts_ns: (123_456_789 + i * 1_000_000_000 / 30) / 1_000 * 1_000,
                ..frame.clone()
            };
            state.push_at(&frame.data, frame.key, frame.pts_ns).unwrap();
            input.push(frame);
        }
        (source, session, state, cleanup, input)
    }
    fn new_capture() -> (
        Arc<Capture>,
        tokio::sync::mpsc::UnboundedReceiver<(u16, u32)>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Arc::new(Capture {
                id: uuid::Uuid::new_v4(),
                tx,
                previous: Mutex::new(None),
            }),
            rx,
        )
    }

    #[tokio::test]
    async fn segmented_disk_and_memory_have_identical_frame_timestamps() {
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            let (_source, session, mut state, _cleanup, input) = recording(42, codec);
            state.stop().unwrap();
            let catalog = segments::list_in(&session.directory).unwrap();
            assert!(catalog.len() >= 4, "catalog: {catalog:?}");
            let mut actual = Vec::new();
            for segment in catalog {
                actual.extend(read_segment(segment).unwrap());
            }
            assert_eq!(actual.len(), input.len());
            for (disk, original) in actual.iter().zip(&input) {
                assert_eq!(disk.pts_ns, session.origin_ms * 1_000_000 + original.pts_ns);
                assert_eq!(disk.key, original.key);
            }
            let recent = session.snapshot();
            assert!(recent.first().unwrap().key);
            assert!(recent.first().unwrap().pts_ns > 10_000_000_000);
            assert!(
                recent.last().unwrap().pts_ns - recent.first().unwrap().pts_ns <= 30_000_000_000
            );
        }
    }

    #[tokio::test]
    async fn segmented_playback_crosses_files_then_recent_buffer_without_rtp_gaps() {
        let (source, session, mut state, _cleanup, input) = recording(42, VideoCodec::H264);
        // Wait for async-finalize to publish completed segments; leave recording active.
        let deadline = Instant::now() + Duration::from_secs(5);
        while segments::list_in(&session.directory).unwrap().len() < 3 {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (capture, mut rx) = new_capture();
        let player = SegmentedPlayer::new(source, session.origin_ms + 9200, capture)
            .await
            .unwrap();
        player.play().await.unwrap();
        let first_index = 270; // keyframe at nine seconds
        let mut previous_sequence = None;
        for original in input.iter().skip(first_index).take(365) {
            let (sequence, timestamp) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let expected = (((session.origin_ms * 1_000_000 + original.pts_ns) as u128 * 90000)
                / 1_000_000_000) as u32;
            assert_eq!(timestamp, expected, "frame at {}", original.pts_ns);
            if let Some(previous) = previous_sequence {
                assert!(sequence != previous);
            }
            previous_sequence = Some(sequence);
        }
        assert!(player.current_timestamp().await >= session.origin_ms + 21000);
        player
            .seek_to_timestamp(session.origin_ms + 40000, 1.0)
            .await
            .unwrap();
        while rx.try_recv().is_ok() {}
        let (_, timestamp) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let expected = (((session.origin_ms * 1_000_000 + input[1170].pts_ns) as u128 * 90000)
            / 1_000_000_000) as u32;
        assert_eq!(timestamp, expected);
        assert!(
            player
                .seek_to_timestamp(session.origin_ms + 100000, 1.0)
                .await
                .is_err()
        );
        player.terminate().await.unwrap();
        state.stop().unwrap();
    }
    #[tokio::test]
    async fn recent_rewind_follows_new_frames_and_finalized_files_survive_restart() {
        let (source, session, mut state, _cleanup, input) = recording(2, VideoCodec::H264);
        assert!(segments::list(&source).unwrap().is_empty());
        let (capture, mut rx) = new_capture();
        let player = SegmentedPlayer::new(source.clone(), session.origin_ms + 500, capture)
            .await
            .unwrap();
        player.play().await.unwrap();
        // Append after playback has captured its initial batch.
        tokio::time::sleep(Duration::from_millis(100)).await;
        for frame in &input {
            state
                .push_at(&frame.data, frame.key, frame.pts_ns + 2_000_000_000)
                .unwrap();
        }
        for i in 0..100u64 {
            let (_, timestamp) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let original = &input[i as usize % input.len()];
            let pts = session.origin_ms * 1_000_000
                + original.pts_ns
                + (i / input.len() as u64) * 2_000_000_000;
            assert_eq!(timestamp, (pts as u128 * 90000 / 1_000_000_000) as u32);
        }
        player.terminate().await.unwrap();
        state.stop().unwrap();
        let origin = session.origin_ms;
        drop(player);
        drop(state);
        drop(session);
        assert!(Session::active(&source).is_none());
        let (capture, mut rx) = new_capture();
        let player = SegmentedPlayer::new(source, origin + 3200, capture)
            .await
            .unwrap();
        player.play().await.unwrap();
        let (_, timestamp) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let pts = origin * 1_000_000 + input[30].pts_ns + 2_000_000_000;
        assert_eq!(timestamp, (pts as u128 * 90000 / 1_000_000_000) as u32);
        player.terminate().await.unwrap();
    }

    #[tokio::test]
    async fn recorder_preserves_source_pts_dts_and_timebase() {
        gst::init().unwrap();
        let source = format!("timestamp-test-{}", uuid::Uuid::new_v4());
        let recorder = DvrRecorder::new(&source).unwrap();
        recorder
            .initialize(&crate::common::FFmpegVideoMetadata {
                codec: VideoCodec::H264,
                extradata: None,
                timebase: crate::common::TimeBase {
                    num: 1,
                    den: 90_000,
                },
            })
            .unwrap();
        let session = Session::active(&source).unwrap();
        let _cleanup = Cleanup(session.directory.parent().unwrap().to_path_buf());
        let frames = fixture_frames(VideoCodec::H264);
        for (index, (pts, dts)) in [(96_000, 90_000), (93_000, 93_000)].into_iter().enumerate() {
            let mut packet = ffmpeg::Packet::copy(&frames[index].data);
            packet.set_pts(Some(pts));
            packet.set_dts(Some(dts));
            if index == 0 {
                packet.set_flags(ffmpeg::packet::Flags::KEY);
            }
            recorder.on_new_packet(Arc::new(packet)).await.unwrap();
        }
        let frames = session.snapshot();
        assert_eq!((frames[0].pts_ns, frames[0].dts_ns), (66_666_000, 0));
        assert_eq!(
            (frames[1].pts_ns, frames[1].dts_ns),
            (33_333_000, 33_333_000)
        );
        recorder.finalize_recording().unwrap();
    }
    /// Optional compatibility matrix: creates encoded video locally and runs it
    /// through the same recorder used for cameras. No network/device is required.
    #[tokio::test]
    #[ignore = "requires x264enc/x265enc; writes temporary compatibility artifacts"]
    async fn camera_compatibility_stress_probe() {
        use std::io::Write;
        gst::init().unwrap();
        let case = std::env::var("DVR_STRESS_CASE").unwrap_or_else(|_| "long45-h264".into());
        let (codec, seconds, encoder) = match case.as_str() {
            "long20-h264" => (
                VideoCodec::H264,
                62,
                "x264enc tune=zerolatency key-int-max=200 option-string=scenecut=0",
            ),
            "long45-h264" => (
                VideoCodec::H264,
                92,
                "x264enc tune=zerolatency key-int-max=450 option-string=scenecut=0",
            ),
            "long45-h265" => (
                VideoCodec::H265,
                92,
                "x265enc tune=zerolatency key-int-max=450 option-string=pools=1:scenecut=0:open-gop=0",
            ),
            "closed-b-h264" => (
                VideoCodec::H264,
                24,
                "x264enc key-int-max=20 bframes=3 option-string=open-gop=0:scenecut=0:b-adapt=0",
            ),
            "closed-b-h265" => (
                VideoCodec::H265,
                24,
                "x265enc key-int-max=20 option-string=pools=1:open-gop=0:scenecut=0:bframes=3:b-adapt=0",
            ),
            "high40-h264" => (
                VideoCodec::H264,
                24,
                "x264enc tune=zerolatency bitrate=40000 nal-hrd=cbr key-int-max=10 option-string=scenecut=0",
            ),
            "high80-gop10-h264" => (
                VideoCodec::H264,
                24,
                "x264enc tune=zerolatency bitrate=80000 nal-hrd=cbr key-int-max=100 option-string=scenecut=0",
            ),
            "open-h264" => (
                VideoCodec::H264,
                24,
                "x264enc key-int-max=20 bframes=3 option-string=open-gop=1:scenecut=0:b-adapt=0",
            ),
            "open-h265" => (
                VideoCodec::H265,
                24,
                "x265enc key-int-max=20 option-string=pools=1:open-gop=1:scenecut=0:bframes=3:b-adapt=0",
            ),
            "high80-h264" => (
                VideoCodec::H264,
                24,
                "x264enc tune=zerolatency bitrate=80000 nal-hrd=cbr key-int-max=10 option-string=scenecut=0",
            ),
            _ => panic!("Unknown stress case {case}"),
        };
        let root = std::path::PathBuf::from(
            std::env::var_os("DVR_STRESS_DIR").unwrap_or_else(|| "/tmp/dvr-camera-check".into()),
        )
        .join(&case);
        std::fs::create_dir_all(&root).unwrap();
        let source = format!("camera-stress-{}", uuid::Uuid::new_v4());
        let session = Session::new(&source, 1_700_000_000_000, codec).unwrap();
        let _cleanup = Cleanup(session.directory.parent().unwrap().to_path_buf());
        let mut recorder = RecordingState::new(
            session.clone(),
            &codec,
            session.origin_ms,
            FramingFormat::AnnexB,
        )
        .unwrap();
        let parser = match codec {
            VideoCodec::H264 => "h264parse",
            VideoCodec::H265 => "h265parse",
        };
        let media_type = match codec {
            VideoCodec::H264 => "h264",
            VideoCodec::H265 => "h265",
        };
        let generator = gst::parse::launch(&format!(
            "videotestsrc pattern=ball num-buffers={} ! video/x-raw,format=I420,width=160,height=96,framerate=10/1 ! {encoder} ! {parser} config-interval=-1 ! video/x-{media_type},stream-format=byte-stream,alignment=au ! appsink name=output sync=false max-buffers=4", seconds*10
        )).unwrap().downcast::<gst::Pipeline>().unwrap();
        let sink = generator
            .by_name("output")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .unwrap();
        generator.set_state(gst::State::Playing).unwrap();
        let mut raw = std::fs::File::create(root.join(format!("source.{media_type}"))).unwrap();
        let mut count = 0u64;
        let mut source_origin = None;
        let mut bytes = 0usize;
        let mut checkpoints = Vec::new();
        loop {
            let sample = sink.try_pull_sample(gst::ClockTime::from_seconds(10));
            let Some(sample) = sample else {
                assert!(sink.is_eos(), "encoder stalled");
                break;
            };
            let buffer = sample.buffer().unwrap();
            let data = buffer.map_readable().unwrap();
            let key = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);
            raw.write_all(data.as_slice()).unwrap();
            bytes += data.len();
            let pts = buffer.pts().unwrap().nseconds();
            let dts = buffer.dts().map(|t| t.nseconds()).unwrap_or(pts);
            let origin = *source_origin.get_or_insert(dts.min(pts));
            recorder
                .push_timed(data.as_slice(), key, pts - origin, dts - origin)
                .unwrap();
            count += 1;
            if [100, 200, 310, 400, 440, 460, 610, 900].contains(&count) {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let recent = session.snapshot();
                let mut rewinds = std::collections::BTreeMap::new();
                for seconds_back in [1, 5, 15, 25] {
                    if count * 100 <= seconds_back * 1000 {
                        continue;
                    }
                    let target_ms = session.origin_ms + count * 100 - seconds_back * 1000;
                    let seek =
                        SegmentedPlayer::new(source.clone(), target_ms, new_capture().0).await;
                    rewinds.insert(
                        seconds_back.to_string(),
                        seek.as_ref()
                            .err()
                            .map(ToString::to_string)
                            .unwrap_or_else(|| "ok".into()),
                    );
                }
                checkpoints.push(serde_json::json!({
                    "elapsed_seconds": count as f64 / 10.0,
                    "rewind_one_second": rewinds["1"],
                    "rewinds_seconds": rewinds,
                    "retained_frames": recent.len(),
                    "retained_bytes": recent.iter().map(|f| f.data.len()).sum::<usize>(),
                    "first_retained_ns": recent.first().map(|f| f.pts_ns),
                    "finalized_segments": segments::list_in(&session.directory).unwrap().len(),
                }));
            }
        }
        generator.set_state(gst::State::Null).unwrap();
        assert_eq!(count, seconds * 10, "encoder produced an incomplete source");
        recorder.stop().unwrap();
        let mut output = Vec::new();
        for segment in segments::list_in(&session.directory).unwrap() {
            let size = std::fs::metadata(&segment.path).unwrap().len();
            let read = read_segment(segment.clone());
            output.push(serde_json::json!({
                "file": segment.path.file_name().unwrap().to_string_lossy(),
                "first_ns": segment.first_ns, "end_ns": segment.end_ns,
                "bytes": size,
                "frames": read.as_ref().ok().map(Vec::len),
                "read_error": read.as_ref().err().map(ToString::to_string),
            }));
            std::fs::copy(&segment.path, root.join(segment.path.file_name().unwrap())).unwrap();
        }
        let result = serde_json::json!({"case": case, "input_frames": count, "input_bytes": bytes,
            "mbps": bytes as f64 * 8.0 / seconds as f64 / 1_000_000.0,
            "checkpoints": checkpoints, "segments": output});
        std::fs::write(
            root.join("results.json"),
            serde_json::to_vec_pretty(&result).unwrap(),
        )
        .unwrap();
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
    }
}
