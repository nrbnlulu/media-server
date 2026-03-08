use crate::app::VideoSourceId;
use crate::common::TimeBase;
use crate::common::traits::{FfmpegConsumer, VideoSource};
use crate::common::{FFmpegVideoMetadata, VideoCodec};
use crate::domain::{StreamConfig, StreamState};
use anyhow::{Result, bail};
use axum::async_trait;
use ffmpeg;
use futures::future::join_all;
use media_server_api_models::VideoSourceInput;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{thread, time};
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::{broadcast, mpsc};

enum LiveStreamState {
    Online,
    Offline,
}

/// Info needed by fallback pipeline to produce timestamp-compatible packets
struct FallbackContext {
    codec: VideoCodec,
    /// Timebase of the live stream (fallback must convert to this)
    live_timebase: ffmpeg::Rational,
    /// Timestamp offset to apply (last_dts + last_duration from live stream)
    ts_offset: i64,
}
pub struct RtspClient {
    config: StreamConfig,
    state: Arc<TokioMutex<StreamState>>,
    codec: Arc<TokioMutex<Option<VideoCodec>>>,
    active_video_input: Mutex<Option<VideoSourceInput>>,
    state_tx: broadcast::Sender<StreamState>,
    shutdown_sig: Arc<AtomicBool>,
    consumers: TokioMutex<Vec<Arc<dyn FfmpegConsumer>>>,
}

impl RtspClient {
    pub fn new(
        config: StreamConfig,
        state_tx: broadcast::Sender<StreamState>,
        consumers: Vec<Arc<dyn FfmpegConsumer>>,
    ) -> Result<Self> {
        validate_rtsp_inputs(&config.rtsp_inputs)?;
        Ok(Self {
            config,
            state: Arc::new(TokioMutex::new(StreamState::Stopped)),
            active_video_input: Mutex::new(None),
            codec: Arc::new(TokioMutex::new(None)),
            state_tx,
            consumers: TokioMutex::new(consumers),
            shutdown_sig: Arc::new(AtomicBool::new(false)),
        })
    }
    pub fn source_id(&self) -> &VideoSourceId {
        &self.config.source_id
    }

    pub async fn add_consumer(&self, consumer: Arc<dyn FfmpegConsumer>) {
        self.consumers.lock().await.push(consumer);
    }

    pub async fn get_codec(&self) -> Option<VideoCodec> {
        *self.codec.lock().await
    }

    async fn set_state(&self, state: StreamState) {
        *self.state.lock().await = state;
        if let Err(e) = self.state_tx.send(state) {
            log::error!("Failed to send state update: {}", e);
        }
    }

    pub async fn state(&self) -> StreamState {
        *self.state.lock().await
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.shutdown_sig.store(true, Ordering::SeqCst);
        for consumer in self.consumers.lock().await.drain(..) {
            if let Err(e) = consumer.finalize().await {
                log::error!("Failed to finalize consumer: {}", e);
            }
        }
        Ok(())
    }

    async fn _execute(self: Arc<Self>) {
        let (live_stream_state_tx, mut live_stream_state_rx) = mpsc::channel::<LiveStreamState>(4);
        let (packet_tx, mut packet_rx) = tokio::sync::mpsc::channel::<ffmpeg::Packet>(30);

        let session_id = self.config.source_id.clone();
        let session_id_for_loop = session_id.clone();
        let (metadata_tx, mut metadata_rx) = tokio::sync::mpsc::channel(1);
        let packet_tx_clone = packet_tx.clone();

        let self_clone = Arc::clone(&self);
        // Spawn the primary FFmpeg pipeline
        tokio::task::spawn_blocking(move || {
            self_clone.run_live_ffmpeg_pipeline(live_stream_state_tx, packet_tx_clone, metadata_tx);
        });

        let session_id = session_id_for_loop;
        let mut fallback_is_running = false;
        let fallback_terminate_sig = Arc::new(AtomicBool::new(true));
        let mut last_dts = 0i64;
        let mut ts_offset = 0i64;
        let mut last_duration = 0i64; // Track for offset calculation during source switching
        let mut current_codec: Option<VideoCodec> = None;
        let mut live_timebase: Option<ffmpeg::Rational> = None;

        loop {
            tokio::select! {
                // Handle State Changes (Online/Offline)
                Some(new_live_state) = live_stream_state_rx.recv() => {
                    match new_live_state {
                        LiveStreamState::Online => {
                            log::info!("Live stream restored for {}, stopping fallback", session_id);
                            // Reset consecutive failures when we get a successful connection
                            fallback_terminate_sig.store(true, Ordering::SeqCst);
                            fallback_is_running = false;
                        },
                        LiveStreamState::Offline => {
                             log::info!("Live stream down for {}, checking fallback state", &session_id);
                             if !fallback_is_running {
                                if let (Some(codec), Some(timebase)) = (current_codec, live_timebase) {
                                    log::info!("Starting fallback pipeline for {} with codec {:?}", &session_id, codec);

                                    // Calculate offset for fallback: continue from where live stream left off
                                    let fallback_ts_offset = last_dts + last_duration;

                                    fallback_terminate_sig.store(false, Ordering::SeqCst);
                                    fallback_is_running = true;

                                    let fallback_packet_tx = packet_tx.clone();
                                    let ctx = FallbackContext {
                                        codec,
                                        live_timebase: timebase,
                                        ts_offset: fallback_ts_offset,
                                    };

                                    let termination_sig_clone = fallback_terminate_sig.clone();
                                    let session_id_clone = session_id.clone();
                                    tokio::task::spawn_blocking(move || {
                                        run_fallback_pipeline(
                                            ctx,
                                            session_id_clone,
                                            termination_sig_clone,
                                            fallback_packet_tx,
                                        );
                                    });
                                } else {
                                    log::debug!("Cannot start fallback for {}: codec or timebase not yet detected", &session_id);
                                }
                            }
                        }
                    }
                }

                // Handle Metadata/Codec Initialization
                Some(metadata) = metadata_rx.recv() => {
                    if current_codec.is_some() {
                        log::warn!("Metadata received after codec was already set; skipping double init");
                        continue;
                    }

                    current_codec = Some(metadata.codec);
                    live_timebase = Some(ffmpeg::Rational::new(metadata.timebase.num, metadata.timebase.den));
                    let consumers = self.consumers.lock().await;
                    for consumer in consumers.iter() {
                        if let Err(err) = consumer.initialize(&metadata) {
                            log::error!("Failed to initialize consumer: {}", err);
                        }
                    }

                    *self.codec.lock().await = Some(metadata.codec);
                    self.set_state(StreamState::Running).await;
                }

                // Handle Incoming Video Packets
                Some(mut packet) = packet_rx.recv() => {
                    let dts = packet.dts().unwrap_or(0);
                    let pts = packet.pts().unwrap_or(dts);
                    let duration = packet.duration();

                   // Rewrite timestamps to be continuous across source switches
                    // Calculate what the new timestamp would be with current offset
                    let mut new_dts = dts + ts_offset;
                    let mut new_pts = pts + ts_offset;

                    // Only adjust offset if the timestamp would still go backwards
                    if new_dts < last_dts && last_dts != 0 {
                        ts_offset = last_dts + last_duration - dts;
                        log::info!("Source switch detected for {}: dts={} would become {} < last_dts={}, adjusting ts_offset to {}",
                            session_id, dts, new_dts, last_dts, ts_offset);

                        // Recalculate with new offset
                        new_dts = dts + ts_offset;
                        new_pts = pts + ts_offset;
                    }

                    packet.set_dts(Some(new_dts));
                    packet.set_pts(Some(new_pts));

                    last_dts = new_dts;
                    last_duration = duration;

                    let consumers = {
                        let guard = self.consumers.lock().await;
                        guard.clone()
                    };

                    let packet_arc = Arc::new(packet);
                    let mut tasks = Vec::with_capacity(consumers.len());
                    for consumer in consumers.iter() {
                        tasks.push(consumer.on_new_packet(packet_arc.clone()));
                    }
                    join_all(tasks).await;
                }
            }
        }
    }

    fn attach_credentials_on_url(&self, url: &mut url::Url) {
        if let (Some(username), Some(password)) = (&self.config.username, &self.config.password) {
            let _ = url.set_username(username);
            let _ = url.set_password(Some(password));
        }
    }

    fn run_live_ffmpeg_pipeline(
        &self,
        state_tx: mpsc::Sender<LiveStreamState>,
        packet_tx: mpsc::Sender<ffmpeg::Packet>,
        metadata_tx: mpsc::Sender<FFmpegVideoMetadata>,
    ) {
        fn real_impl(
            self_: &RtspClient,
            stream_input: &VideoSourceInput,
            state_tx: &mpsc::Sender<LiveStreamState>,
            packet_tx: &mpsc::Sender<ffmpeg::Packet>,
            metadata_tx: &mpsc::Sender<FFmpegVideoMetadata>,
        ) -> anyhow::Result<()> {
            let url = &stream_input.url;
            let mut opts = ffmpeg::Dictionary::new();
            let source_id = self_.source_id();
            let shutdown_sig = &self_.shutdown_sig;
            opts.set("rtsp_transport", "tcp");
            opts.set("stimeout", "1000000"); // Socket timeout (1 second in microseconds)
            opts.set("timeout", "1000000"); // I/O timeout (1 second)
            opts.set("rw_timeout", "1000000"); // Read/write timeout (1 second)
            opts.set("max_delay", "500000"); // Max demux delay (0.5 seconds)
            opts.set("reorder_queue_size", "500"); // Small reorder queue for packet reordering
            opts.set("analyzeduration", "1000000");
            opts.set("probesize", "1000000");
            opts.set("fflags", "nobuffer+discardcorrupt");
            opts.set("flags", "low_delay");
            opts.set("err_detect", "explode"); // Exit on errors instead of retrying

            log::info!("Opening RTSP stream: {}", url);
            let mut ictx = ffmpeg::format::input_with_dictionary(url.as_str(), opts)?;

            let video_stream = ictx
                .streams()
                .best(ffmpeg::media::Type::Video)
                .ok_or_else(|| anyhow::anyhow!("No video stream found"))?;

            let video_stream_index = video_stream.index();
            let codec_id = video_stream.parameters().id();

            let detected_codec = match codec_id {
                ffmpeg::codec::Id::H264 => VideoCodec::H264,
                ffmpeg::codec::Id::HEVC => VideoCodec::H265,
                other => bail!("Unsupported codec: {:?}", other),
            };

            log::info!(
                "Detected codec: {:?} for stream {}",
                detected_codec,
                source_id
            );
            let parameters = video_stream.parameters();
            let extradata = extract_extradata(&parameters);
            let timebase = TimeBase::from_ffmpeg(video_stream.time_base())?;
            let video_metadata = FFmpegVideoMetadata {
                codec: detected_codec,
                extradata,
                timebase,
            };
            // Try to send metadata - it's ok if the receiver already has it (channel full)
            if let Err(e) = metadata_tx.try_send(video_metadata) {
                log::error!(
                    "Failed to send video metadata for stream {}: {}",
                    source_id,
                    e
                );
                bail!("Failed to send video metadata");
            }

            log::info!("RTSP stream opened for stream {}", source_id);

            let mut should_emit_state_online = true;
            loop {
                if shutdown_sig.load(Ordering::SeqCst) {
                    log::info!("Shutdown signal received for stream {}", source_id);
                    break;
                }

                match ictx.next_packet() {
                    Ok(packet) => {
                        if should_emit_state_online {
                            should_emit_state_online = false;
                            if let Err(err) = state_tx.blocking_send(LiveStreamState::Online) {
                                log::error!("Failed to send online state: {}", err);
                            }
                            self_.set_active_video_input(stream_input.clone());
                        }
                        if packet.stream() != video_stream_index || packet.is_corrupt() {
                            continue;
                        }

                        if let Err(e) = packet_tx.blocking_send(packet) {
                            log::error!("Failed to send packet: {}", e);
                            break;
                        }
                    }
                    Err(ffmpeg::Error::Eof) => {
                        log::info!("EOF reached for stream {}", source_id);
                        bail!("EOF reached");
                    }
                    Err(e) => {
                        log::warn!("Failed to read packet for stream {}: {}", source_id, e);
                        bail!("Failed to read packet");
                    }
                }
            }

            log::info!("RTSP packet loop exited for stream {}", self_.source_id());
            Ok(())
        }

        let mut current_stream_input_index = 0;
        let shutdown_sig = &self.shutdown_sig;
        let restart_delay = Duration::from_secs(self.config.restart_interval_secs.unwrap_or(3));
        let source_id = self.source_id();
        let mut inputs: Vec<VideoSourceInput> = self
            .config
            .rtsp_inputs
            .clone()
            .into_iter()
            .map(|mut i| {
                self.attach_credentials_on_url(&mut i.url);
                i
            })
            .collect();
        inputs.sort_by(|a, b| a.priority.cmp(&b.priority));
        let inputs = inputs;
        let mut have_tested_all_inputs = false;

        loop {
            if shutdown_sig.load(Ordering::SeqCst) {
                log::info!("Shutdown signal received for stream {}", source_id);
                break;
            }

            let input;
            let active_video_input = self.active_input();
            if let Some(active_input) = active_video_input {
                input = active_input;
            } else {
                input = match inputs.get(current_stream_input_index) {
                    Some(input) => input.clone(),
                    None => {
                        have_tested_all_inputs = true;
                        current_stream_input_index = 0;
                        inputs[0].clone()
                    }
                };
                current_stream_input_index += 1;
            }

            match real_impl(self, &input, &state_tx, &packet_tx, &metadata_tx) {
                Ok(_) => {}
                Err(e) => {
                    log::warn!("RTSP stream for {} failed due to: {}", source_id, e);
                    if let Err(e) = state_tx.try_send(LiveStreamState::Offline) {
                        log::error!("Failed to send state update: {}", e);
                    }
                }
            };

            // this will allow us to use restart delay only if we have already tested all inputs
            // otherwise if we prioritize a stream that is not available, the next one that should work
            // would only be tested after the restart delay
            if have_tested_all_inputs || self.active_input().is_some() {
                thread::sleep(restart_delay);
            }
        }
    }

    fn set_active_video_input(&self, source: VideoSourceInput) {
        let mut guard = self.active_video_input.lock();
        *guard = Some(source);
    }
}

pub fn validate_rtsp_inputs(rtsp_inputs: &[VideoSourceInput]) -> Result<()> {
    if rtsp_inputs.is_empty() {
        anyhow::bail!("At least one RTSP input must be provided");
    }

    for input in rtsp_inputs {
        if input.url.scheme() != "rtsp" && input.url.scheme() != "rtsps" {
            anyhow::bail!(
                "Invalid URL scheme '{}'. Must be rtsp:// or rtsps://",
                input.url.scheme()
            );
        }
    }

    Ok(())
}

/// Convert a timestamp from one timebase to another.
/// Formula: ts_in_target = ts_in_source * (source_tb.num / source_tb.den) / (target_tb.num / target_tb.den)
///        = ts_in_source * source_tb.num * target_tb.den / (source_tb.den * target_tb.num)
fn rescale_ts(ts: i64, from_tb: ffmpeg::Rational, to_tb: ffmpeg::Rational) -> i64 {
    let from_num = from_tb.numerator() as i128;
    let from_den = from_tb.denominator() as i128;
    let to_num = to_tb.numerator() as i128;
    let to_den = to_tb.denominator() as i128;

    // Avoid division by zero
    if from_den == 0 || to_num == 0 {
        log::warn!("rescale_ts: invalid timebase, returning original timestamp");
        return ts;
    }

    // Use i128 to avoid overflow during multiplication
    let num = ts as i128 * from_num * to_den;
    let den = from_den * to_num;
    (num / den) as i64
}

fn run_fallback_pipeline(
    ctx: FallbackContext,
    session_id: VideoSourceId,
    terminate_sig: Arc<AtomicBool>,
    packet_tx: mpsc::Sender<ffmpeg::Packet>,
) {
    let codec_str = format!("{:?}", ctx.codec).to_lowercase();
    let file_path = format!("assets/placeholder_{}.mp4", codec_str);

    log::info!(
        "Starting fallback pipeline for {} using {}, ts_offset={}, live_timebase={}/{}",
        session_id,
        file_path,
        ctx.ts_offset,
        ctx.live_timebase.numerator(),
        ctx.live_timebase.denominator()
    );

    let mut ictx = match ffmpeg::format::input(&file_path) {
        Ok(ctx) => ctx,
        Err(e) => {
            log::error!("Fallback file error for {}: {}", session_id, e);
            return;
        }
    };

    let stream = match ictx.streams().best(ffmpeg::media::Type::Video) {
        Some(s) => s,
        None => {
            log::error!("No video stream in fallback file for {}", session_id);
            return;
        }
    };
    let stream_index = stream.index();
    let file_timebase = stream.time_base();

    log::debug!(
        "Fallback file timebase: {}/{}, live timebase: {}/{}",
        file_timebase.numerator(),
        file_timebase.denominator(),
        ctx.live_timebase.numerator(),
        ctx.live_timebase.denominator()
    );

    // Track cumulative offset across file loops (in live timebase units)
    let mut cumulative_offset = ctx.ts_offset;
    let mut last_converted_dts = ctx.ts_offset;

    loop {
        // Reset playback timing for each loop iteration
        let start_time = time::Instant::now();

        loop {
            if terminate_sig.load(Ordering::SeqCst) {
                log::info!("Fallback terminated for {} (terminate signal)", session_id);
                return;
            }

            match ictx.next_packet() {
                Ok(mut packet) => {
                    if packet.stream() != stream_index {
                        continue;
                    }

                    // Throttle: Wait until it's time to send this packet (real-time playback)
                    // Use original file timestamps for timing calculation
                    if let Some(dts) = packet.dts() {
                        let actual_ts = (dts as f64 * f64::from(file_timebase.numerator()))
                            / f64::from(file_timebase.denominator());
                        let elapsed = start_time.elapsed().as_secs_f64();
                        if actual_ts > elapsed {
                            thread::sleep(time::Duration::from_secs_f64(actual_ts - elapsed));
                        }
                    }

                    // Convert timestamps from file timebase to live stream timebase
                    let orig_dts = packet.dts().unwrap_or(0);
                    let orig_pts = packet.pts().unwrap_or(orig_dts);

                    let converted_dts =
                        rescale_ts(orig_dts, file_timebase, ctx.live_timebase) + cumulative_offset;
                    let converted_pts =
                        rescale_ts(orig_pts, file_timebase, ctx.live_timebase) + cumulative_offset;
                    let converted_duration =
                        rescale_ts(packet.duration(), file_timebase, ctx.live_timebase);

                    packet.set_dts(Some(converted_dts));
                    packet.set_pts(Some(converted_pts));
                    packet.set_duration(converted_duration);

                    last_converted_dts = converted_dts;

                    if packet_tx.blocking_send(packet).is_err() {
                        log::info!("Fallback terminated for {} (channel closed)", session_id);
                        return;
                    }
                }
                Err(ffmpeg::Error::Eof) => {
                    // End of file - update cumulative offset before seeking back
                    cumulative_offset = last_converted_dts + 1; // +1 to ensure continuity
                    log::debug!(
                        "Fallback file finished for {}, seeking to start, new cumulative_offset={}",
                        session_id,
                        cumulative_offset
                    );
                    if let Err(e) = ictx.seek(0, 0..i64::MAX) {
                        log::error!("Failed to seek fallback file for {}: {}", session_id, e);
                        return;
                    }
                    break; // Break inner loop to restart with fresh timing
                }
                Err(e) => {
                    log::error!("Failed to read fallback packet for {}: {}", session_id, e);
                    return;
                }
            }

            if terminate_sig.load(Ordering::SeqCst) {
                log::info!("Fallback terminated for {} (terminate signal)", session_id);
                return;
            }
        }
    }
}

fn extract_extradata(params: &ffmpeg::codec::Parameters) -> Option<Vec<u8>> {
    unsafe {
        let ptr = params.as_ptr();
        if ptr.is_null() {
            return None;
        }
        let size = (*ptr).extradata_size;
        if size <= 0 {
            return None;
        }
        let data = (*ptr).extradata;
        if data.is_null() {
            return None;
        }
        Some(std::slice::from_raw_parts(data, size as usize).to_vec())
    }
}

#[async_trait]
impl VideoSource for RtspClient {
    async fn execute(self: Arc<Self>) {
        self._execute().await
    }

    async fn codec(&self) -> Option<VideoCodec> {
        *self.codec.lock().await
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.shutdown().await
    }

    fn inputs(&self) -> &[VideoSourceInput] {
        &self.config.rtsp_inputs
    }

    fn active_input(&self) -> Option<VideoSourceInput> {
        self.active_video_input.lock().clone()
    }

    fn source_id(&self) -> &VideoSourceId {
        &self.config.source_id
    }

    async fn state(&self) -> StreamState {
        *self.state.lock().await
    }

    fn config(&self) -> &StreamConfig {
        &self.config
    }

    fn subscribe_state(&self) -> broadcast::Receiver<StreamState> {
        self.state_tx.subscribe()
    }
}
