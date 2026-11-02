use super::segments::{Frame, Segment};
use anyhow::{Result, anyhow, bail, ensure};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_app as gst_app;
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};
const MAX_SEGMENT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("Active DVR file has no complete media fragment yet")]
pub(crate) struct NotReady;

// Expose only complete top-level MP4 boxes through the last complete mdat.
// This is a bounded byte view, not a synthetic MP4 index or footer.
pub(crate) fn readable_len(path: &std::path::Path) -> Result<u64> {
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
    if complete == 0 {
        return Err(NotReady.into());
    }
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
pub(crate) struct SegmentReader {
    _lease: super::maintenance::SegmentLease,
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
    pub(crate) segment: Segment,
    first_pts: Option<u64>,
    pub(crate) opened_len: u64,
    pub(crate) exhausted: bool,
}
impl Drop for SegmentReader {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
impl SegmentReader {
    pub(crate) fn open(segment: Segment) -> Result<Self> {
        segment.validate()?;
        let lease = super::maintenance::SegmentLease::acquire(&segment.path);
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
            crate::common::VideoCodec::H264 => "h264parse",
            crate::common::VideoCodec::H265 => "h265parse",
        })
        .property("config-interval", -1i32)
        .build()?;
        let caps = gst::Caps::builder(match codec {
            crate::common::VideoCodec::H264 => "video/x-h264",
            crate::common::VideoCodec::H265 => "video/x-h265",
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
            _lease: lease,
            pipeline,
            sink,
            opened_len,
            segment,
            first_pts: None,
            exhausted: false,
        })
    }
    pub(crate) fn next(&mut self) -> Result<Option<Frame>> {
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
                    key: super::segments::independent_frame(data.as_slice(), self.segment.codec()?),
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
    pub(crate) fn batch(&mut self) -> Result<VecDeque<Frame>> {
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
