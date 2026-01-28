use crate::app::ClientSessionId;
use crate::common::nal_utils::{self, H264NalType, H265NalType};
use crate::common::traits::{FfmpegConsumer, RtpConsumer};
use crate::common::{FFmpegVideoMetadata, TimeBase, VideoCodec};
use anyhow::{anyhow, bail};
use axum::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use futures::future::join_all;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU16, AtomicU32, Ordering};
use std::sync::Arc;

/// RTP header fields that can be modified per-consumer without copying payload.
/// Based on RFC 3550 RTP header structure.
#[derive(Debug, Clone)]
pub struct RtpHeader {
    /// Payload type (7 bits)
    pub payload_type: u8,
    /// Marker bit - typically indicates end of frame
    pub marker: bool,
    /// Sequence number (16 bits)
    pub seq: u16,
    /// Timestamp (32 bits) - media clock
    pub timestamp: u32,
    /// Synchronization source identifier (32 bits)
    pub ssrc: u32,
}

impl RtpHeader {
    /// Serialize header to 12-byte RTP header format.
    /// Version=2, no padding, no extension, no CSRC.
    #[inline]
    pub fn to_bytes(&self) -> [u8; 12] {
        let mut buf = [0u8; 12];
        // Byte 0: V=2, P=0, X=0, CC=0 => 0x80
        buf[0] = 0x80;
        // Byte 1: M + PT
        buf[1] = (if self.marker { 0x80 } else { 0 }) | (self.payload_type & 0x7F);
        // Bytes 2-3: sequence number
        buf[2] = (self.seq >> 8) as u8;
        buf[3] = self.seq as u8;
        // Bytes 4-7: timestamp
        buf[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        // Bytes 8-11: SSRC
        buf[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        buf
    }

    /// Parse header from raw RTP packet bytes.
    /// Returns None if packet is too short.
    pub fn from_bytes(packet: &[u8]) -> Option<Self> {
        if packet.len() < 12 {
            return None;
        }
        Some(Self {
            payload_type: packet[1] & 0x7F,
            marker: (packet[1] & 0x80) != 0,
            seq: u16::from_be_bytes([packet[2], packet[3]]),
            timestamp: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
            ssrc: u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
        })
    }
}

/// RTP frame with header separated from payload for zero-copy stitching.
/// The payload is shared (Arc) across all consumers; only headers are cloned/modified.
#[derive(Debug, Clone)]
pub struct RtpPacket {
    pub header: RtpHeader,
    /// Payload data (without RTP header). Shared across consumers.
    pub payload: Arc<[u8]>,
}

impl RtpPacket {
    /// Create a new RtpFrame from header and payload.
    pub fn new(header: RtpHeader, payload: Arc<[u8]>) -> Self {
        Self { header, payload }
    }

    /// Serialize to complete RTP packet bytes (header + payload).
    pub fn to_bytes(&self) -> Vec<u8> {
        let header_bytes = self.header.to_bytes();
        let mut packet = Vec::with_capacity(12 + self.payload.len());
        packet.extend_from_slice(&header_bytes);
        packet.extend_from_slice(&self.payload);
        packet
    }

    /// Total packet size (header + payload)
    #[inline]
    pub fn len(&self) -> usize {
        12 + self.payload.len()
    }
}

struct SrcStreamState {
    codec: VideoCodec,
    parsed_extradata: nal_utils::ParsedExtraData,
    codec_params: CodecParameters,
    timebase: TimeBase,
}

// according to gemini 1428 should cover most cases including 4g phones
const DEFAULT_MTU: usize = 1428;
const RTP_HEADER_SIZE: usize = 12;

pub struct RtpPacketizer {
    ssrc: u32,
    sequence: AtomicU16,
    payload_type: u8,
    mtu: usize,
    clock_rate: u32,
    stream_metadata: Mutex<Option<SrcStreamState>>,
    is_initialized: AtomicBool,
    consumers: Mutex<Vec<Arc<dyn RtpConsumer>>>,
}

impl RtpPacketizer {
    pub fn new(ssrc: u32, payload_type: u8) -> Self {
        Self {
            ssrc,
            sequence: AtomicU16::new(rand::random()),
            payload_type,
            mtu: DEFAULT_MTU,
            clock_rate: 90000,
            stream_metadata: Mutex::new(None),
            is_initialized: AtomicBool::new(false),
            consumers: Mutex::new(Vec::new()),
        }
    }

    pub fn get_codec_params(&self) -> Option<CodecParameters> {
        self.stream_metadata
            .lock()
            .as_ref()
            .map(|metadata| metadata.codec_params.clone())
    }

    pub fn add_consumer(&self, consumer: Arc<dyn RtpConsumer>) {
        let mut consumers_lock = self.consumers.lock();
        if !consumers_lock.contains(&consumer) {
            consumers_lock.push(consumer);
        }
    }

    pub fn remove_consumer(&self, consumer: &dyn RtpConsumer) {
        let mut consumers_lock = self.consumers.lock();
        if let Some(index) = consumers_lock.iter().position(|c| c.id() == consumer.id()) {
            consumers_lock.remove(index);
        }
    }

    pub fn with_mtu(mut self, mtu: usize) -> Self {
        self.mtu = mtu;
        self
    }

    /// Packetize NAL units into RtpFrames (header + shared payload).
    pub fn packetize(
        &self,
        nal_units: &[Vec<u8>],
        timestamp: u32,
        codec: &VideoCodec,
    ) -> Vec<RtpPacket> {
        let mut rtp_frames = Vec::new();
        let nal_count = nal_units.len();

        for (i, nal) in nal_units.iter().enumerate() {
            if nal.is_empty() {
                continue;
            }

            let is_last_nal = i == nal_count - 1;
            let frames = match codec {
                VideoCodec::H264 => self.packetize_h264(nal, is_last_nal, timestamp),
                VideoCodec::H265 => self.packetize_h265(nal, is_last_nal, timestamp),
            };
            rtp_frames.extend(frames);
        }

        rtp_frames
    }

    fn packetize_h264(&self, nal: &[u8], is_last_nal: bool, timestamp: u32) -> Vec<RtpPacket> {
        let max_payload = self.mtu - RTP_HEADER_SIZE;

        if nal.len() <= max_payload {
            vec![self.build_single_nal_frame(nal, is_last_nal, timestamp)]
        } else {
            self.build_h264_fu_a_frames(nal, is_last_nal, timestamp)
        }
    }

    fn packetize_h265(&self, nal: &[u8], is_last_nal: bool, timestamp: u32) -> Vec<RtpPacket> {
        let max_payload = self.mtu - RTP_HEADER_SIZE;

        if nal.len() <= max_payload {
            vec![self.build_single_nal_frame(nal, is_last_nal, timestamp)]
        } else {
            self.build_h265_fu_frames(nal, is_last_nal, timestamp)
        }
    }

    fn next_seq(&self) -> u16 {
        self.sequence.fetch_add(1, Ordering::Relaxed)
    }

    fn build_single_nal_frame(&self, nal: &[u8], marker: bool, timestamp: u32) -> RtpPacket {
        let header = RtpHeader {
            payload_type: self.payload_type,
            marker,
            seq: self.next_seq(),
            timestamp,
            ssrc: self.ssrc,
        };
        RtpPacket::new(header, Arc::from(nal))
    }

    fn build_h264_fu_a_frames(
        &self,
        nal: &[u8],
        is_last_nal: bool,
        timestamp: u32,
    ) -> Vec<RtpPacket> {
        let mut frames = Vec::new();

        let nal_header = nal[0];
        let nal_type = nal_header & 0x1F;
        let nri = nal_header & 0x60;

        let fu_indicator = 28 | nri;

        let payload_data = &nal[1..];
        let max_fragment_size = self.mtu - RTP_HEADER_SIZE - 2;

        let chunks: Vec<&[u8]> = payload_data.chunks(max_fragment_size).collect();
        let chunk_count = chunks.len();

        for (i, chunk) in chunks.into_iter().enumerate() {
            let is_first = i == 0;
            let is_last_fragment = i == chunk_count - 1;

            let mut fu_header = nal_type;
            if is_first {
                fu_header |= 0x80;
            }
            if is_last_fragment {
                fu_header |= 0x40;
            }

            let mut fu_payload = Vec::with_capacity(2 + chunk.len());
            fu_payload.push(fu_indicator);
            fu_payload.push(fu_header);
            fu_payload.extend_from_slice(chunk);

            let marker = is_last_nal && is_last_fragment;

            let header = RtpHeader {
                payload_type: self.payload_type,
                marker,
                seq: self.next_seq(),
                timestamp,
                ssrc: self.ssrc,
            };
            frames.push(RtpPacket::new(header, Arc::from(fu_payload)));
        }

        frames
    }

    fn build_h265_fu_frames(
        &self,
        nal: &[u8],
        is_last_nal: bool,
        timestamp: u32,
    ) -> Vec<RtpPacket> {
        let mut frames = Vec::new();

        if nal.len() < 2 {
            return frames;
        }

        let nal_header = u16::from_be_bytes([nal[0], nal[1]]);
        let nal_type = ((nal_header >> 9) & 0x3F) as u8;
        let layer_id = ((nal_header >> 3) & 0x3F) as u8;
        let tid = (nal_header & 0x07) as u8;

        let fu_type: u8 = 49;
        let fu_header_byte1 = (fu_type << 1) | (layer_id >> 5);
        let fu_header_byte2 = ((layer_id & 0x1F) << 3) | tid;

        let payload_data = &nal[2..];
        let max_fragment_size = self.mtu - RTP_HEADER_SIZE - 3;

        let chunks: Vec<&[u8]> = payload_data.chunks(max_fragment_size).collect();
        let chunk_count = chunks.len();

        for (i, chunk) in chunks.into_iter().enumerate() {
            let is_first = i == 0;
            let is_last_fragment = i == chunk_count - 1;

            let mut fu_header = nal_type;
            if is_first {
                fu_header |= 0x80;
            }
            if is_last_fragment {
                fu_header |= 0x40;
            }

            let mut fu_payload = Vec::with_capacity(3 + chunk.len());
            fu_payload.push(fu_header_byte1);
            fu_payload.push(fu_header_byte2);
            fu_payload.push(fu_header);
            fu_payload.extend_from_slice(chunk);

            let marker = is_last_nal && is_last_fragment;

            let header = RtpHeader {
                payload_type: self.payload_type,
                marker,
                seq: self.next_seq(),
                timestamp,
                ssrc: self.ssrc,
            };
            frames.push(RtpPacket::new(header, Arc::from(fu_payload)));
        }

        frames
    }

    pub fn pts_to_rtp_timestamp(&self, pts: i64, timebase: &TimeBase) -> u32 {
        if !timebase.is_valid() {
            return 0;
        }
        let rtp_ts = (pts * self.clock_rate as i64 * timebase.num as i64) / timebase.den as i64;
        rtp_ts as u32
    }
}

#[async_trait]
impl FfmpegConsumer for RtpPacketizer {
    fn initialize(&self, metadata: &FFmpegVideoMetadata) -> anyhow::Result<()> {
        if self.is_initialized.load(Ordering::Relaxed) {
            bail!("RtpPacketizer is already initialized");
        }

        let ffmpeg_extradata = metadata
            .extradata
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no extradata"))?;

        if ffmpeg_extradata.is_empty() {
            bail!("empty extradata");
        }

        let parsed_extradata = match metadata.codec {
            VideoCodec::H264 => nal_utils::parse_h264_extradata(ffmpeg_extradata),
            VideoCodec::H265 => nal_utils::parse_h265_extradata(ffmpeg_extradata),
        }
        .ok_or_else(|| anyhow!("failed to parse extradata"))?;

        let mut codec_params = CodecParameters::default();

        // Set the NAL length size if it's AVCC
        if let nal_utils::FramingFormat::Avcc { length_size } = parsed_extradata.framing_format {
            codec_params.nal_length_size = Some(length_size);
        }

        for nal in parsed_extradata.nals.iter() {
            codec_params.update_from_nal(&metadata.codec, nal);
        }

        let stream_state = SrcStreamState {
            codec: metadata.codec.clone(),
            parsed_extradata,
            codec_params,
            timebase: metadata.timebase.clone(),
        };

        log::info!(
            "RTP packetizer initialized with time_base={:?}",
            stream_state.timebase
        );

        self.stream_metadata.lock().replace(stream_state);
        self.is_initialized.store(true, Ordering::SeqCst);

        Ok(())
    }

    async fn on_new_packet(&self, packet: Arc<ffmpeg::Packet>) -> anyhow::Result<()> {
        if !self.is_initialized.load(Ordering::Relaxed) {
            log::warn!("RtpPacketizer: received packet but not initialized yet");
            return Ok(());
        }

        // Get time_base from stored metadata (packet.time_base() is often 0/1)
        let timebase = {
            let guard = self.stream_metadata.lock();
            let state = guard.as_ref().ok_or(anyhow!("not initialized"))?;
            state.timebase.clone()
        };

        let pts = packet.pts();
        let dts = packet.dts();
        let timestamp_src = match pts {
            Some(0) => dts,
            Some(pts) => Some(pts),
            None => dts,
        }
        .unwrap_or(0);

        let timestamp = self.pts_to_rtp_timestamp(timestamp_src, &timebase);

        let (codec, nal_units) = {
            let mut config_guard = self.stream_metadata.lock();
            let config = config_guard.as_mut().ok_or(anyhow!("not configured"))?;
            let mut nal_units = None;
            // If data exists, check for SPS/PPS updates before we release the lock
            if let Some(data) = packet.data() {
                // Try to detect the framing format from the actual packet data
                // FFmpeg RTSP often sends Annex-B even when extradata parsing suggested AVCC
                let nal_units_ = if nal_utils::is_annex_b(data) {
                    nal_utils::parse_annex_b(data)
                } else {
                    nal_utils::parse_nal_units_with_length(
                        data,
                        &config.parsed_extradata.framing_format,
                    )
                };

                for nal in &nal_units_ {
                    // Update params in-band (e.g., resolution change)
                    config.codec_params.update_from_nal(&config.codec, nal);
                }
                nal_units = Some(nal_units_);
            }

            (config.codec.clone(), nal_units)
        };

        if let Some(nal_units) = nal_units {
            if !nal_units.is_empty() {
                let rtp_frames = self.packetize(&nal_units, timestamp, &codec);
                let consumers = self.consumers.lock().clone();

                if consumers.is_empty() {
                    return Ok(());
                }

                // Send frames directly to consumers without spawning tasks
                // This reduces overhead significantly for real-time streaming
                for frame in rtp_frames {
                    let arc_frame = Arc::new(frame);
                    let mut tasks = Vec::new();
                    for consumer in &consumers {
                        tasks.push(consumer.on_new_packet(arc_frame.clone()));
                    }
                    join_all(tasks).await;
                }
            }
        }
        Ok(())
    }
    async fn finalize(&self) -> anyhow::Result<()> {
        let consumers = self.consumers.lock().clone();
        let mut task_set = tokio::task::JoinSet::new();
        for consumer in &consumers {
            let c = consumer.clone();
            task_set.spawn(async move {
                let _ = c.finalize().await;
            });
        }
        while let Some(_) = task_set.join_next().await {}
        Ok(())
    }
}

/// Rewrites RTP headers to maintain sequence/timestamp continuity across source switches.
///
/// Useful for:
/// - DVR / Live switching
/// - Always-up mechanism (placeholder video when camera is offline)
/// - Seamless failover between redundant sources
///
/// Auto-detects source switches by monitoring for large timestamp discontinuities
/// and automatically adjusts offsets to maintain smooth playback.
pub struct RtpStitcher {
    ssrc: u32,
    seq: AtomicU16,
    ts_offset: AtomicI64,
    last_output_ts: AtomicU32,
    last_input_ts: AtomicU32,
    has_received_packet: AtomicBool,
}

impl RtpStitcher {
    pub fn new(ssrc: u32) -> Self {
        Self {
            ssrc,
            seq: AtomicU16::new(rand::random()),
            ts_offset: AtomicI64::new(0),
            last_output_ts: AtomicU32::new(0),
            last_input_ts: AtomicU32::new(0),
            has_received_packet: AtomicBool::new(false),
        }
    }

    /// Rewrite RTP header fields (seq, timestamp, ssrc) while sharing payload (zero-copy).
    /// Returns a new RtpFrame with updated header but same payload Arc.
    ///
    /// Automatically detects source switches via timestamp discontinuities and
    /// adjusts offsets to maintain smooth playback.
    pub fn stitch(&self, frame: &RtpPacket) -> RtpPacket {
        let orig_ts = frame.header.timestamp;

        // Check for source switch: large timestamp discontinuity indicates new source
        // Normal inter-frame delta at 30fps/90kHz is ~3000, so anything > 1 second (90000)
        // of discontinuity likely indicates a source switch
        if self.has_received_packet.load(Ordering::Relaxed) {
            let last_input = self.last_input_ts.load(Ordering::Relaxed);
            let delta = if orig_ts >= last_input {
                orig_ts - last_input
            } else {
                // Handle wraparound or backwards jump
                last_input - orig_ts
            };

            // If delta > 1 second of RTP time, assume source switch
            const SOURCE_SWITCH_THRESHOLD: u32 = 90000; // 1 second at 90kHz
            if delta > SOURCE_SWITCH_THRESHOLD {
                log::debug!(
                    "RtpStitcher: detected source switch (delta={}), adjusting offset",
                    delta
                );
                self.adjust_for_source_switch(orig_ts);
            }
        } else {
            self.has_received_packet.store(true, Ordering::Relaxed);
        }

        self.last_input_ts.store(orig_ts, Ordering::Relaxed);

        // Calculate new timestamp with offset
        let offset = self.ts_offset.load(Ordering::Relaxed);
        let new_ts = if offset >= 0 {
            orig_ts.wrapping_add(offset as u32)
        } else {
            orig_ts.wrapping_sub((-offset) as u32)
        };
        self.last_output_ts.store(new_ts, Ordering::Relaxed);

        // Create new header with stitched values, share the same payload Arc
        let new_header = RtpHeader {
            payload_type: frame.header.payload_type,
            marker: frame.header.marker,
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            timestamp: new_ts,
            ssrc: self.ssrc,
        };

        RtpPacket {
            header: new_header,
            payload: frame.payload.clone(), // Arc clone = cheap reference increment
        }
    }

    /// Manually trigger source switch adjustment.
    /// Call when switching sources (live→DVR, seek, failover).
    /// Adjusts ts_offset so output timestamps continue smoothly from where we left off.
    ///
    /// `new_source_first_ts`: The first RTP timestamp from the new source
    pub fn adjust_for_source_switch(&self, new_source_first_ts: u32) {
        let last_ts = self.last_output_ts.load(Ordering::Relaxed);
        // We want: new_source_first_ts + offset = last_ts + small_delta
        // So: offset = last_ts + small_delta - new_source_first_ts
        // Using a small delta (e.g., 1 frame at 30fps = 3000 ticks at 90kHz)
        const FRAME_DELTA: u32 = 3000;
        let target_ts = last_ts.wrapping_add(FRAME_DELTA);
        let offset = target_ts as i64 - new_source_first_ts as i64;
        self.ts_offset.store(offset, Ordering::Relaxed);
        log::debug!(
            "RtpStitcher: adjusted offset to {} (last_output={}, new_input={})",
            offset,
            last_ts,
            new_source_first_ts
        );
    }

    /// Reset the stitcher state (e.g., on new client connection)
    pub fn reset(&self) {
        self.seq.store(rand::random(), Ordering::Relaxed);
        self.ts_offset.store(0, Ordering::Relaxed);
        self.last_output_ts.store(0, Ordering::Relaxed);
        self.last_input_ts.store(0, Ordering::Relaxed);
        self.has_received_packet.store(false, Ordering::Relaxed);
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }
}

/// Wraps any RtpConsumer with RTP header stitching for seamless source switching.
/// Zero-copy: only modifies headers, payload Arc is shared.
pub struct StitchingConsumer {
    id: ClientSessionId,
    stitcher: RtpStitcher,
    inner: Arc<dyn RtpConsumer>,
}

impl StitchingConsumer {
    pub fn new(inner: Arc<dyn RtpConsumer>) -> Self {
        Self {
            id: *inner.id(),
            stitcher: RtpStitcher::new(rand::random()),
            inner,
        }
    }

    /// Call when switching sources to maintain timestamp continuity
    pub fn adjust_for_source_switch(&self, new_source_first_ts: u32) {
        self.stitcher.adjust_for_source_switch(new_source_first_ts);
    }

    /// Access the inner consumer
    pub fn inner(&self) -> &Arc<dyn RtpConsumer> {
        &self.inner
    }
}

#[async_trait]
impl RtpConsumer for StitchingConsumer {
    fn id(&self) -> &ClientSessionId {
        &self.id
    }

    async fn on_new_packet(&self, frame: Arc<RtpPacket>) {
        let stitched = self.stitcher.stitch(&frame);
        self.inner.on_new_packet(Arc::new(stitched)).await;
    }

    async fn finalize(&self) -> anyhow::Result<()> {
        self.inner.finalize().await
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct CodecParameters {
    /// Video Parameter Set, unique to h265, some metadata stuff.
    pub vps: Option<Vec<u8>>,
    /// Sequence Parameter Set, framerate, color format, res.
    pub sps: Option<Vec<u8>>,
    /// Picture Parameter Set, can be changed mid-stream, contains some metadata for the codec.
    pub pps: Option<Vec<u8>>,
    pub nal_length_size: Option<usize>,
}

impl CodecParameters {
    pub fn update_from_nal(&mut self, codec: &VideoCodec, nal: &[u8]) {
        if nal.is_empty() {
            return;
        }

        match codec {
            VideoCodec::H264 => {
                if let Some(nal_type) = nal_utils::get_h264_nal_type(nal) {
                    match nal_type {
                        H264NalType::Sps => self.sps = Some(nal.to_vec()),
                        H264NalType::Pps => self.pps = Some(nal.to_vec()),
                        _ => {}
                    }
                }
            }
            VideoCodec::H265 => {
                if let Some(nal_type) = nal_utils::get_h265_nal_type(nal) {
                    match nal_type {
                        H265NalType::Vps => self.vps = Some(nal.to_vec()),
                        H265NalType::Sps => self.sps = Some(nal.to_vec()),
                        H265NalType::Pps => self.pps = Some(nal.to_vec()),
                        _ => {}
                    }
                }
            }
        }
    }

    pub fn update_from_extradata(&mut self, codec: &VideoCodec, extradata: &[u8]) {
        if extradata.is_empty() {
            return;
        }

        if let Some(parsed) = match codec {
            VideoCodec::H264 => nal_utils::parse_h264_extradata(extradata),
            VideoCodec::H265 => nal_utils::parse_h265_extradata(extradata),
        } {
            self.nal_length_size = match parsed.framing_format {
                nal_utils::FramingFormat::AnnexB => None,
                nal_utils::FramingFormat::Avcc { length_size } => Some(length_size),
            };
            for nal in parsed.nals {
                self.update_from_nal(codec, &nal);
            }
            return;
        }

        if nal_utils::is_annex_b(extradata) {
            for nal in nal_utils::parse_annex_b(extradata) {
                self.update_from_nal(codec, &nal);
            }
        }
    }

    pub fn fmtp(&self, codec: &VideoCodec) -> String {
        match codec {
            VideoCodec::H264 => {
                let mut parts = vec!["packetization-mode=1".to_string()];
                if let (Some(sps), Some(pps)) = (&self.sps, &self.pps) {
                    let sps_b64 = BASE64_STANDARD.encode(sps);
                    let pps_b64 = BASE64_STANDARD.encode(pps);
                    parts.push(format!("sprop-parameter-sets={sps_b64},{pps_b64}"));
                }
                parts.join(";")
            }
            VideoCodec::H265 => {
                // In HEVC over RTP, the Decoding Order Number (DON)
                // is used to manage packets that arrive out of order or streams that use complex interleaving.
                // This stands for "Stream Property Max DON Difference."
                // It defines the maximum absolute difference between the DON
                // of any two NAL units that occur in the same transmission order.
                let mut parts = vec!["sprop-max-don-diff=0".to_string()];
                if let Some(vps) = &self.vps {
                    parts.push(format!("sprop-vps={}", BASE64_STANDARD.encode(vps)));
                }
                if let Some(sps) = &self.sps {
                    parts.push(format!("sprop-sps={}", BASE64_STANDARD.encode(sps)));
                }
                if let Some(pps) = &self.pps {
                    parts.push(format!("sprop-pps={}", BASE64_STANDARD.encode(pps)));
                }
                parts.join(";")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_small_nal_single_packet() {
        let packetizer = RtpPacketizer::new(12345, 96);
        let nal = vec![0x67, 0x42, 0x00, 0x1e];

        let frames = packetizer.packetize(&[nal.clone()], 0, &VideoCodec::H264);
        assert_eq!(frames.len(), 1);

        // Payload should match the NAL unit
        assert_eq!(&*frames[0].payload, &nal[..]);
        assert_eq!(frames[0].header.ssrc, 12345);
        assert_eq!(frames[0].header.payload_type, 96);
    }

    #[test]
    fn test_large_nal_fragmentation() {
        let packetizer = RtpPacketizer::new(12345, 96).with_mtu(100);
        let nal = vec![0x65; 500];

        let frames = packetizer.packetize(&[nal], 0, &VideoCodec::H264);
        assert!(frames.len() > 1);

        // First FU-A packet: FU indicator + FU header with Start bit
        let first_payload = &frames[0].payload;
        assert_eq!(first_payload[0] & 0x1F, 28); // FU-A type
        assert!(first_payload[1] & 0x80 != 0); // Start bit set
        assert!(first_payload[1] & 0x40 == 0); // End bit not set

        // Last FU-A packet: End bit set
        let last_payload = &frames[frames.len() - 1].payload;
        assert!(last_payload[1] & 0x80 == 0); // Start bit not set
        assert!(last_payload[1] & 0x40 != 0); // End bit set
        assert!(frames[frames.len() - 1].header.marker); // Marker bit set
    }

    #[test]
    fn test_rtp_frame_serialization() {
        let header = RtpHeader {
            payload_type: 96,
            marker: true,
            seq: 1234,
            timestamp: 90000,
            ssrc: 0xDEADBEEF,
        };
        let payload: Arc<[u8]> = Arc::from(vec![1, 2, 3, 4]);
        let frame = RtpPacket::new(header, payload);

        let bytes = frame.to_bytes();
        assert_eq!(bytes.len(), 16); // 12 header + 4 payload

        // Parse it back
        let parsed = RtpHeader::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.payload_type, 96);
        assert!(parsed.marker);
        assert_eq!(parsed.seq, 1234);
        assert_eq!(parsed.timestamp, 90000);
        assert_eq!(parsed.ssrc, 0xDEADBEEF);
    }

    #[test]
    fn test_update_from_h264_extradata() {
        let sps = vec![0x67, 0x42, 0x00, 0x1e];
        let pps = vec![0x68, 0xce, 0x3c, 0x80];
        let extradata = vec![
            0x01, 0x64, 0x00, 0x1e, 0xff, 0xe1, 0x00, 0x04, 0x67, 0x42, 0x00, 0x1e, 0x01, 0x00,
            0x04, 0x68, 0xce, 0x3c, 0x80,
        ];

        let mut params = CodecParameters::default();
        params.update_from_extradata(&VideoCodec::H264, &extradata);

        assert_eq!(params.nal_length_size, Some(4));
        assert_eq!(params.sps, Some(sps));
        assert_eq!(params.pps, Some(pps));
    }

    #[test]
    fn test_update_from_h265_extradata() {
        let vps = vec![0x40, 0x01, 0x0c];
        let sps = vec![0x42, 0x01, 0x01, 0x60];
        let pps = vec![0x44, 0x01, 0xc0];

        let mut extradata = vec![
            0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1e, 0xf0,
            0x00, 0xfc, 0xfc, 0xf8, 0xf8, 0x00, 0x00, 0x03, 0x03,
        ];

        push_h265_array(&mut extradata, 32, &vps);
        push_h265_array(&mut extradata, 33, &sps);
        push_h265_array(&mut extradata, 34, &pps);

        let mut params = CodecParameters::default();
        params.update_from_extradata(&VideoCodec::H265, &extradata);

        assert_eq!(params.nal_length_size, Some(4));
        assert_eq!(params.vps, Some(vps));
        assert_eq!(params.sps, Some(sps));
        assert_eq!(params.pps, Some(pps));
    }

    fn push_h265_array(buf: &mut Vec<u8>, nal_type: u8, nal: &[u8]) {
        buf.push(0x80 | (nal_type & 0x3f));
        buf.extend_from_slice(&1u16.to_be_bytes());
        buf.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        buf.extend_from_slice(nal);
    }
}
