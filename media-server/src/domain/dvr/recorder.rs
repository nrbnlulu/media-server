use crate::common::nal_utils::{self, FramingFormat};
use crate::common::traits::FfmpegConsumer;
use crate::common::{TimeBase, VideoCodec};
use crate::domain::dvr::filesystem;
use crate::utils::UnixTimestamp;
use anyhow::{bail, Result};
use axum::async_trait;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use parking_lot::Mutex;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

struct RecordingState {
    framing_format: FramingFormat,
    file_path: PathBuf,
    start_time: UnixTimestamp,
    pipeline: gst::Pipeline,
    appsrc: gst_app::AppSrc,
    codec: VideoCodec,
    waiting_for_keyframe: bool,
    header_buffer: Vec<Vec<u8>>,
    stream_started: bool,
    timebase: TimeBase,
}

impl RecordingState {
    fn new(
        path: &PathBuf,
        codec: &VideoCodec,
        start_time: UnixTimestamp,
        framing_format: FramingFormat,
        timebase: TimeBase,
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

        let mux = gst::ElementFactory::make("isofmp4mux")
            .build()
            .map_err(|_| anyhow::anyhow!("Failed to create isofmp4mux"))?;
        mux.set_property("fragment-duration", gst::ClockTime::from_seconds(1));
        // Use first buffer's timestamp as start time, since RTSP streams
        // often have timestamps that don't start at 0
        mux.set_property_from_str("start-time-selection", "first");
        let sink = gst::ElementFactory::make("filesink")
            .build()
            .map_err(|_| anyhow::anyhow!("Failed to create filesink"))?;
        sink.set_property("location", path.to_str().unwrap());
        sink.set_property("sync", false);

        pipeline.add_many(&[&appsrc, &parser, &mux, &sink])?;
        appsrc
            .link(&parser)
            .map_err(|_| anyhow::anyhow!("Failed to link appsrc to parser"))?;
        parser
            .link(&mux)
            .map_err(|_| anyhow::anyhow!("Failed to link parser to mux"))?;
        mux.link(&sink)
            .map_err(|_| anyhow::anyhow!("Failed to link mux to sink"))?;

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| anyhow::anyhow!("Failed to set pipeline to playing state"))?;

        let appsrc_typed = appsrc
            .dynamic_cast::<gst_app::AppSrc>()
            .map_err(|_| anyhow::anyhow!("Failed to cast to AppSrc"))?;

        Ok(Self {
            pipeline,
            file_path: path.clone(),
            codec: codec.clone(),
            waiting_for_keyframe: true,
            start_time,
            appsrc: appsrc_typed,
            header_buffer: Vec::new(),
            stream_started: false,
            framing_format,
            timebase,
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
        // emit EOS event to the pipeline
        let _ = self.appsrc.end_of_stream()?;
        let bus = self
            .pipeline
            .bus()
            .ok_or_else(|| anyhow::anyhow!("Pipeline has no bus"))?;
        // wait for EOS or error message
        if let Some(msg) = bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(5),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            match msg.view() {
                gst::MessageView::Error(err) => {
                    eprintln!("Error during EOS: {}", err.error());
                }
                gst::MessageView::Eos(_) => {
                    println!("Stream finished successfully.");
                }
                _ => (),
            }
        } else {
            println!("Shutdown timed out after 5 seconds.");
        }

        self.pipeline
            .set_state(gst::State::Null)
            .map_err(|_| anyhow::anyhow!("Failed to set pipeline to null state"))?;

        self.waiting_for_keyframe = true;
        self.header_buffer.clear();
        self.stream_started = false;
        Ok(())
    }

    fn push_buffer_to_appsrc(
        &self,
        data: &[u8],
        pts: i64,
        dts: i64,
        timebase: &TimeBase,
        is_keyframe: bool,
    ) -> Result<()> {
        let pts_ns = if pts >= 0 {
            let numerator = (pts as u64)
                .saturating_mul(timebase.num as u64)
                .saturating_mul(1_000_000_000);
            numerator / (timebase.den as u64)
        } else {
            0
        };
        let dts_ns = if dts >= 0 {
            let numerator = (dts as u64)
                .saturating_mul(timebase.num as u64)
                .saturating_mul(1_000_000_000);
            numerator / (timebase.den as u64)
        } else {
            pts_ns
        };

        let mut buffer = gst::Buffer::from_slice(data.to_vec());
        {
            let buffer_ref = buffer.get_mut().unwrap();
            buffer_ref.set_pts(gst::ClockTime::from_nseconds(pts_ns));
            buffer_ref.set_dts(gst::ClockTime::from_nseconds(dts_ns));

            if !is_keyframe {
                buffer_ref.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }

        match self.appsrc.push_buffer(buffer) {
            Ok(_) => Ok(()),
            Err(gst::FlowError::Flushing) => Ok(()),
            Err(e) => bail!("Failed to push buffer: {:?}", e),
        }
    }

    fn write_nal_data(
        &mut self,
        nal_units: &[Vec<u8>],
        pts: i64,
        dts: i64,
        timebase: &TimeBase,
        is_key_frame: bool,
    ) -> Result<()> {
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

        if self.waiting_for_keyframe && !header_nals.is_empty() {
            self.header_buffer.extend(header_nals);
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
            self.push_buffer_to_appsrc(&combined_bytes, pts, dts, timebase, true)?;

            self.waiting_for_keyframe = false;
            self.header_buffer.clear();
            return Ok(());
        }

        let packet_bytes = nal_utils::build_annex_b(nal_units);
        self.push_buffer_to_appsrc(&packet_bytes, pts, dts, timebase, is_key_frame)?;

        Ok(())
    }
}

pub struct DvrRecorder {
    stream_id: u64,
    recording_state: Mutex<Option<RecordingState>>,
}

impl DvrRecorder {
    pub fn new(stream_id: u64) -> Result<Self> {
        Ok(Self {
            stream_id,
            recording_state: Mutex::new(None),
        })
    }

    fn initialize_imp(
        &self,
        codec: VideoCodec,
        extradata: Option<&Vec<u8>>,
        timebase: TimeBase,
    ) -> Result<PathBuf> {
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

        log::info!(
            "DVR recorder using framing format: {:?}, time_base: {:?}",
            framing_format,
            timebase
        );

        let start_time = crate::utils::get_current_unix_timestamp();
        let dir = filesystem::ensure_stream_dvr_dir(self.stream_id)?;
        let filename = filesystem::generate_active_recording_filename(start_time);
        let path = dir.join(filename);

        let mut new_state =
            RecordingState::new(&path, &codec, start_time, framing_format, timebase)?;

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
            let old_path = state.file_path.clone();
            let start_time = state.start_time;

            state.stop()?;

            if !old_path.exists() {
                log::warn!("Recording file does not exist: {:?}", old_path);
                return Ok(None);
            }

            let end_time = crate::utils::get_current_unix_timestamp();
            let filename = filesystem::generate_finished_recording_filename(start_time, end_time);
            let new_path = old_path.parent().unwrap().join(filename);

            if let Err(e) = fs::rename(&old_path, &new_path) {
                log::error!("Failed to rename DVR recording file: {}", e);
                return Err(e.into());
            }

            log::info!(
                "Finalized recording for stream {}: {:?}",
                self.stream_id,
                new_path
            );

            return Ok(Some(new_path));
        }

        Ok(None)
    }

    pub fn current_path(&self) -> Option<PathBuf> {
        self.recording_state
            .lock()
            .as_ref()
            .map(|s| s.file_path.clone())
    }

    pub fn stream_id(&self) -> u64 {
        self.stream_id
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
        self.initialize_imp(
            metadata.codec,
            metadata.extradata.as_ref(),
            metadata.timebase.clone(),
        )?;
        Ok(())
    }

    async fn on_new_packet(&self, packet: Arc<ffmpeg::Packet>) -> Result<()> {
        let mut state_guard = self.recording_state.lock();

        if let Some(state) = state_guard.as_mut() {
            if let Some(data) = packet.data() {
                let is_key = packet.is_key();
                let nal_units = state.parse_nal_units(data);
                let pts = packet.pts().unwrap_or(0);
                let dts = packet.dts().unwrap_or(pts);
                let duration = packet.duration();
                // Use stored timebase from stream metadata, not packet.time_base()
                // which often returns 0/1 for packets
                let timebase = state.timebase.clone();
                state.write_nal_data(&nal_units, pts, dts, &timebase, is_key)?;
            }
        }

        Ok(())
    }

    async fn finalize(&self) -> Result<()> {
        self.finalize_recording()?;
        Ok(())
    }
}
