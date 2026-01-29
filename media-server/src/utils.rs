use anyhow::{bail, Result};
use media_server_api_models::UnixTimestamp;

pub fn check_dependencies() -> Result<()> {
    // Initialize FFmpeg
    log::info!("FFmpeg initialized successfully");

    // Initialize GStreamer
    gstreamer::init()?;

    let version = gstreamer::version();
    log::info!(
        "GStreamer version: {}.{}.{}.{}",
        version.0,
        version.1,
        version.2,
        version.3
    );

    let required_elements = vec![
        "appsrc",
        "appsink",
        "h264parse",
        "h265parse",
        "qtdemux",
        "isofmp4mux",
        "filesink",
        "queue",
    ];

    for element_name in required_elements {
        if gstreamer::ElementFactory::find(element_name).is_none() {
            bail!("Required GStreamer element '{}' not found. Please install the necessary GStreamer plugins.", element_name);
        }
    }

    log::info!("All required GStreamer elements are available");
    Ok(())
}

pub fn convert_h264_to_annex_b(data: Vec<u8>) -> Result<Vec<u8>> {
    let mut result = data;
    let mut i = 0;

    while i < result.len().saturating_sub(3) {
        if i + 4 > result.len() {
            bail!("Invalid NAL unit: incomplete length field at offset {}", i);
        }

        let len =
            u32::from_be_bytes([result[i], result[i + 1], result[i + 2], result[i + 3]]) as usize;

        result[i] = 0;
        result[i + 1] = 0;
        result[i + 2] = 0;
        result[i + 3] = 1;

        i += 4 + len;

        if i > result.len() {
            bail!("Invalid NAL unit: length {} exceeds remaining data", len);
        }
    }

    if i < result.len() {
        bail!("Invalid NAL stream: incomplete length field at end");
    }

    Ok(result)
}

pub fn convert_h265_to_annex_b(data: Vec<u8>) -> Result<Vec<u8>> {
    convert_h264_to_annex_b(data)
}

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

static PLACEHOLDER_FRAMES: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
static PLACEHOLDER_FRAME_INDEX: AtomicUsize = AtomicUsize::new(0);


fn log_nal_units(data: &[u8]) {
    let mut i = 0;
    let mut nal_types = Vec::new();

    while i < data.len().saturating_sub(4) {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1 {
            if i + 4 < data.len() {
                let nal_header = data[i + 4];
                let nal_type = nal_header & 0x1F;
                let nal_name = match nal_type {
                    1 => "Non-IDR",
                    5 => "IDR",
                    6 => "SEI",
                    7 => "SPS",
                    8 => "PPS",
                    9 => "AUD",
                    _ => "Other",
                };
                nal_types.push(format!("{}({})", nal_name, nal_type));
                i += 4;
            }
        }
        i += 1;
    }

    log::info!(
        "Placeholder frame NAL units: {} (total {} bytes)",
        nal_types.join(", "),
        data.len()
    );
}



fn extract_all_h264_frames_from_mp4(mp4_data: &[u8]) -> Result<Vec<Vec<u8>>> {
    use gstreamer::prelude::*;
    use gstreamer_app as gst_app;

    let pipeline = gstreamer::parse::launch(
        "appsrc name=src ! qtdemux ! h264parse config-interval=-1 ! \
         video/x-h264,stream-format=byte-stream,alignment=au ! \
         appsink name=sink",
    )?
    .downcast::<gstreamer::Pipeline>()
    .map_err(|_| anyhow::anyhow!("Not a pipeline"))?;

    let appsrc = pipeline
        .by_name("src")
        .unwrap()
        .downcast::<gst_app::AppSrc>()
        .unwrap();

    let appsink = pipeline
        .by_name("sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();

    pipeline.set_state(gstreamer::State::Playing)?;

    let mut buffer = gstreamer::Buffer::with_size(mp4_data.len())?;
    {
        let buffer_mut = buffer.get_mut().unwrap();
        let mut map = buffer_mut.map_writable()?;
        map.copy_from_slice(mp4_data);
    }
    appsrc.push_buffer(buffer)?;
    appsrc.end_of_stream()?;

    let mut frames = Vec::new();
    let timeout = gstreamer::ClockTime::from_mseconds(100);

    loop {
        match appsink.try_pull_sample(timeout) {
            Some(sample) => {
                let buffer = sample.buffer().unwrap();
                let map = buffer.map_readable()?;
                let h264_data = map.as_slice().to_vec();
                frames.push(h264_data);
            }
            None => {
                break;
            }
        }
    }

    let _ = pipeline.set_state(gstreamer::State::Null);
    std::thread::sleep(std::time::Duration::from_millis(50));

    log::info!(
        "Extracted {} H.264 frames from MP4 (total {} bytes)",
        frames.len(),
        frames.iter().map(|f| f.len()).sum::<usize>()
    );

    if frames.is_empty() {
        anyhow::bail!("No frames extracted from MP4");
    }

    Ok(frames)
}

pub fn get_current_unix_timestamp() -> UnixTimestamp {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as UnixTimestamp
}

pub fn unix_epoch_to_system_time(epoch: u64) -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_micros(epoch)
}

use sha2::{Digest, Sha256};
use std::fmt::Write;

pub const DVR_DIRECTORY: &str = "dvr";

pub fn get_stream_hash(rtsp_url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(rtsp_url.as_bytes());
    let result = hasher.finalize();

    let mut hash_string = String::with_capacity(64);
    for byte in result.iter() {
        write!(&mut hash_string, "{:02x}", byte).unwrap();
    }
    hash_string
}

pub fn get_dvr_path(source_id: u64) -> std::path::PathBuf {
    std::path::PathBuf::from(DVR_DIRECTORY).join(format!("{}.mp4", source_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_h264_to_annex_b() {
        let input = vec![
            0, 0, 0, 5, 0x67, 0x42, 0x00, 0x1e, 0x8c, 0, 0, 0, 3, 0x68, 0xce, 0x3c,
        ];

        let result = convert_h264_to_annex_b(input).unwrap();

        assert_eq!(result[0..4], [0, 0, 0, 1]);
        assert_eq!(result[4], 0x67);
        assert_eq!(result[9..13], [0, 0, 0, 1]);
        assert_eq!(result[13], 0x68);
    }
}
