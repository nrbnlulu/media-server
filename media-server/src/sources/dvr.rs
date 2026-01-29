use crate::app::VideoSourceId;
use crate::common::nal_utils;
use crate::common::rtp::RtpPacketizer;
use crate::common::traits::RtpConsumer;
use crate::common::VideoCodec;
use crate::domain::dvr::filesystem::{self, FindNextRecRes, RecordingMetadata};
use media_server_api_models::UnixTimestamp;
use anyhow::{anyhow, bail, Result};
use chrono::Duration;
use futures::StreamExt;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{self as gst_app, AppSinkCallbacks};
use parking_lot::Mutex;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::mpsc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use tokio::sync::broadcast::Sender;
use tokio::sync::{broadcast, watch};

struct CurrentPipelineState {
    pipeline: gst::Pipeline,
    bus: gst::Bus,
    speed: f64,
    recording_metadata: RecordingMetadata,
}

impl CurrentPipelineState {
    async fn stop(&self) -> anyhow::Result<()> {
        let _ = self.pipeline.send_event(gst::event::Eos::new());
        let mut bus_stream = self.bus.stream();
        while let Some(msg) = bus_stream.next().await {
            match msg.view() {
                gst::MessageView::Eos(_) => break,
                _ => continue,
            }
        }
        self.pipeline
            .set_state(gst::State::Null)
            .map_err(|e| anyhow!("failed to stop pipeline {e}"))?;
        Ok(())
    }

    async fn play(&self) -> anyhow::Result<()> {
        self.pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| anyhow!("failed to play pipeline {e}"))?;
        Ok(())
    }
}

enum DvrPlayerState {
    Scheduled(Duration),
    Playing(CurrentPipelineState),
}

pub struct DvrPlayer {
    codec: VideoCodec,
    state: Arc<tokio::sync::Mutex<CurrentPipelineState>>,
    source_id: VideoSourceId,
    consumer: Arc<dyn RtpConsumer>,
    reset_state_chan: (
        tokio::sync::mpsc::Sender<()>,
        tokio::sync::mpsc::Receiver<()>,
    ),
}

impl DvrPlayer {
    pub fn new(
        source_id: VideoSourceId,
        initial_start_time: UnixTimestamp,
        // FIXME: this should be deduced automatically based on the file (we can also do some file name convention for this)
        codec: VideoCodec,
        consumer: Arc<dyn RtpConsumer>,
    ) -> Result<Self> {
        let initial_state = Self::resolve_new_state(
            source_id,
            filesystem::find_next_recording(source_id, initial_start_time),
            initial_start_time,
            consumer.clone(),
            &codec,
        )?;
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
        if let Some(clock_time) = state_guard.pipeline.current_clock_time() {
            if let Some(base_time) = state_guard.pipeline.base_time() {
                let running_time = clock_time.saturating_sub(base_time);
                return start_time + running_time.nseconds();
            }
        }
        start_time
    }

    pub async fn current_time_ms(&self) -> Option<u64> {
        let state_guard = self.state.lock().await;
        if let Some(pos) = state_guard.pipeline.query_position::<gst::ClockTime>() {
            Some(pos.mseconds())
        } else {
            None
        }
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
        source_id: VideoSourceId,
        res: FindNextRecRes,
        initial_start_time: UnixTimestamp,
        consumer: Arc<dyn RtpConsumer>,
        codec: &VideoCodec,
    ) -> anyhow::Result<CurrentPipelineState> {
        match res {
            Some((recording, None)) => {
                let (pipeline, bus) =
                    create_pipeline(&recording, initial_start_time, codec, consumer)?;
                Ok(CurrentPipelineState {
                    pipeline,
                    bus,
                    speed: 1.0,
                    recording_metadata: recording,
                })
            }
            // FIXME: maybe we should wait until the recording is available?
            _ => bail!("No recording found for source ID {}", source_id),
        }
    }

    pub async fn play(&self) {
        loop {
            let join_handle = {
                let bus = {
                    let state = self.state.lock().await;
                    if let Err(e) = state.play().await {
                        log::error!("Failed to play pipeline: {}", e);
                        return;
                    }
                    // wait for EOS task
                    state.bus.clone()
                };
                let join_handle = tokio::spawn(async move {
                    let mut bus_stream = bus.stream();
                    while let Some(msg) = bus_stream.next().await {
                        match msg.view() {
                            gst::MessageView::Eos(_) => {
                                // check the TODO.md
                                log::warn!("EOS is not expected");
                                break;
                            }
                            _ => (),
                        }
                    }
                });
                join_handle
            };
            join_handle.await;
        }
    }

    pub async fn terminate(&self) -> Result<()> {
        let state_guard = self.state.lock().await;
        state_guard.stop().await
    }

    pub async fn seek_to_timestamp(&self, timestamp: u64, _speed: f64) -> Result<(), SeekError> {
        {
            let state_guard = self.state.lock().await;

            if timestamp < state_guard.recording_metadata.start_time {
                return Err(SeekError::SeekBeforeStart);
            }
            if let Some(end_time) = state_guard.recording_metadata.end_time {
                if timestamp > end_time {
                    return Err(SeekError::SeekAfterEnd);
                }
            }
            let offset_ms = timestamp.saturating_sub(state_guard.recording_metadata.start_time);
            let start = gst::ClockTime::from_mseconds(offset_ms);
            log::debug!("DVR seek to {} (offset_ms={})", timestamp, offset_ms);
            state_guard
                .pipeline
                .seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, start)
                .map_err(|e| anyhow::anyhow!(format!("Seek failed: {}", e)))
                .map_err(|e| SeekError::GstError(e))?;
        }
        Ok(())
    }
}

fn create_pipeline(
    recording: &RecordingMetadata,
    initial_time: UnixTimestamp,
    // FIXME: we know that based on the current source (rtsp) codec
    // FIXME: thus if it changed somehow mid stream we're doomed.
    src_codec: &VideoCodec,
    consumer: Arc<dyn RtpConsumer>,
) -> Result<(gst::Pipeline, gst::Bus)> {
    let path_str = recording.path.to_string_lossy().to_string();
    log::info!("Opening DVR file: {}", path_str);

    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("filesrc")
        .build()
        .map_err(|_| anyhow::anyhow!("Failed to create filesrc"))?;
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

    src.set_property("location", path_str);

    // Note: No RTP payloader - we output raw H264/H265 and packetize ourselves
    pipeline.add_many(&[
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
        if let Some(caps) = src_pad.current_caps() {
            if let Some(structure) = caps.structure(0) {
                if !structure.name().starts_with("video/") {
                    return;
                }
            }
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
    let codec_clone = src_codec.clone();

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
            if frame_count <= 5 || frame_count % 100 == 0 {
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
    let bus = pipeline.bus().ok_or(anyhow!("no bust"))?;
    Ok((pipeline, bus))
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

