//! Normalize a downloaded recording through the same segment writer as live DVR.
use super::{
    recorder::RecordingState,
    segments::{self, Session},
};
use crate::common::{
    VideoCodec, extract_extradata,
    nal_utils::{self, FramingFormat},
};
use anyhow::{Result, anyhow, bail, ensure};
use serde::Serialize;
use std::path::Path;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize)]
pub struct ImportResult {
    pub actual_start_ms: u64,
    pub actual_end_ms: u64,
    pub codec: String,
}

pub fn result_in(directory: &Path) -> Result<Option<ImportResult>> {
    result_from_segments(segments::list_in(directory)?)
}

pub(crate) fn result_from_segments(files: Vec<segments::Segment>) -> Result<Option<ImportResult>> {
    if files.is_empty() {
        return Ok(None);
    }
    let codec = files[0].codec.clone();
    ensure!(
        files.iter().all(|s| s.codec == codec),
        "corrupt: mixed video codecs"
    );
    Ok(Some(ImportResult {
        actual_start_ms: files.iter().map(|s| s.start_ms()).min().unwrap(),
        actual_end_ms: files.iter().map(|s| s.end_ms()).max().unwrap(),
        codec,
    }))
}

fn ns(value: i64, base: ffmpeg::Rational) -> Result<i64> {
    ensure!(
        base.numerator() > 0 && base.denominator() > 0,
        "corrupt: invalid time base"
    );
    i64::try_from(
        i128::from(value) * i128::from(base.numerator()) * 1_000_000_000
            / i128::from(base.denominator()),
    )
    .map_err(|_| anyhow!("corrupt: timestamp overflow"))
}

fn open_media(input: &Path, cancel: &CancellationToken) -> Result<ffmpeg::format::context::Input> {
    let cancel = cancel.clone();
    ffmpeg::format::input_with_interrupt(input, move || cancel.is_cancelled())
        .map_err(|_| anyhow!("corrupt: cannot open downloaded media"))
}

fn read_packet(input: &mut ffmpeg::format::context::Input) -> Result<Option<ffmpeg::Packet>> {
    let mut packet = ffmpeg::Packet::empty();
    match packet.read(input) {
        Ok(()) => Ok(Some(packet)),
        Err(ffmpeg::Error::Eof) => Ok(None),
        Err(_) => bail!("corrupt: media demux failed"),
    }
}

fn codec_frame_rate(
    input: &Path,
    index: usize,
    parameters: ffmpeg::codec::Parameters,
    cancel: &CancellationToken,
) -> Result<Option<ffmpeg::Rational>> {
    let mut context = ffmpeg::codec::Context::from_parameters(parameters)?;
    context.set_threading(ffmpeg::codec::threading::Config::count(1));
    let mut decoder = context.decoder().video()?;
    let mut demux = open_media(input, cancel)?;
    let mut probes = 0;
    while let Some(packet) = read_packet(&mut demux)? {
        ensure!(!cancel.is_cancelled(), "cancelled");
        if packet.stream() != index {
            continue;
        }
        let _ = decoder.send_packet(&packet);
        let mut frame = ffmpeg::frame::Video::empty();
        let _ = decoder.receive_frame(&mut frame);
        if let Some(rate) = decoder
            .frame_rate()
            .filter(|r| r.numerator() > 0 && r.denominator() > 0)
        {
            return Ok(Some(rate));
        }
        probes += 1;
        if probes >= 32 {
            break;
        }
    }
    Ok(None)
}

pub fn normalize(
    input: &Path,
    directory: &Path,
    origin_ms: u64,
    cancel: &CancellationToken,
) -> Result<ImportResult> {
    ffmpeg::init()?;
    gstreamer::init()?;
    let mut demux = open_media(input, cancel)?;
    let stream = demux
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or_else(|| anyhow!("no_footage: no video track"))?;
    let index = stream.index();
    let base = stream.time_base();
    let codec = match stream.parameters().id() {
        ffmpeg::codec::Id::H264 => VideoCodec::H264,
        ffmpeg::codec::Id::HEVC => VideoCodec::H265,
        _ => bail!("unsupported_codec: only H.264 and H.265 are supported"),
    };
    let nominal_fps = if stream.avg_frame_rate().numerator() > 0 {
        stream.avg_frame_rate()
    } else {
        stream.rate()
    };
    let parameters = stream.parameters();
    let extradata = extract_extradata(&parameters);
    let parsed = extradata.as_ref().and_then(|data| match codec {
        VideoCodec::H264 => nal_utils::parse_h264_extradata(data),
        VideoCodec::H265 => nal_utils::parse_h265_extradata(data),
    });
    let (framing, mut headers) = parsed
        .map(|p| (p.framing_format, p.nals))
        .unwrap_or((FramingFormat::AnnexB, Vec::new()));
    let mut timings = Vec::new();
    while let Some(packet) = read_packet(&mut demux)? {
        ensure!(!cancel.is_cancelled(), "cancelled");
        if packet.stream() == index {
            timings.push((packet.pts(), packet.dts()));
            ensure!(timings.len() <= 120_000, "corrupt: too many video packets");
        }
    }
    ensure!(!timings.is_empty(), "no_footage: empty video track");
    let retime = timings
        .iter()
        .any(|(pts, dts)| pts.is_none() || dts.is_none())
        || timings.windows(2).any(|p| p[0].1 >= p[1].1);
    // Raw demuxers can report a default 25 fps despite different SPS timing.
    // Only the timestamp-repair path probes a frame to prefer codec timing.
    let fps = if retime {
        codec_frame_rate(input, index, parameters, cancel)
            .ok()
            .flatten()
            .unwrap_or(nominal_fps)
    } else {
        nominal_fps
    };
    if retime {
        ensure!(
            fps.numerator() > 0 && fps.denominator() > 0,
            "corrupt: no frame rate for timestamp reconstruction"
        );
    }
    let first_pts = if retime {
        0
    } else {
        ns(timings[0].0.unwrap(), base)?
    };
    let first_dts = if retime {
        0
    } else {
        ns(timings[0].1.unwrap(), base)?
    };
    let epoch = first_pts.min(first_dts);
    let shift_ms = u64::try_from((i128::from(first_pts) - i128::from(epoch)) / 1_000_000)?;
    let recording_origin = origin_ms
        .checked_sub(shift_ms)
        .ok_or_else(|| anyhow!("corrupt: recording origin underflow"))?;
    let mut writer: Option<RecordingState> = None;
    let mut session = None;
    let mut demux = open_media(input, cancel)?;
    let mut frame = 0usize;
    let mut last_dts = None;
    let mut written = 0usize;
    while let Some(packet) = read_packet(&mut demux)? {
        ensure!(!cancel.is_cancelled(), "cancelled");
        if packet.stream() != index {
            continue;
        }
        let data = packet
            .data()
            .ok_or_else(|| anyhow!("corrupt: empty video packet"))?;
        let nals = if nal_utils::is_annex_b(data) {
            nal_utils::parse_annex_b(data)
        } else {
            nal_utils::parse_nal_units_with_length(data, &framing)
        };
        ensure!(!nals.is_empty(), "corrupt: invalid video packet framing");
        for nal in &nals {
            if nal_utils::is_parameter_set(nal, &codec) {
                let kind = |n: &[u8]| {
                    if codec == VideoCodec::H264 {
                        n[0] & 31
                    } else {
                        (n[0] >> 1) & 63
                    }
                };
                headers.retain(|h| kind(h) != kind(nal));
                headers.push(nal.clone());
            }
        }
        let bytes = nal_utils::build_annex_b(&nals);
        let key = segments::independent_frame(&bytes, codec);
        let (pts, dts) = if retime {
            let timestamp = i64::try_from(
                frame as i128 * 1_000_000_000 * i128::from(fps.denominator())
                    / i128::from(fps.numerator()),
            )?;
            (i128::from(timestamp), i128::from(timestamp))
        } else {
            (
                i128::from(ns(packet.pts().unwrap(), base)?) - i128::from(epoch),
                i128::from(ns(packet.dts().unwrap(), base)?) - i128::from(epoch),
            )
        };
        frame += 1;
        if writer.is_none() {
            if !key {
                continue;
            }
            let required: &[u8] = if codec == VideoCodec::H264 {
                &[7, 8]
            } else {
                &[32, 33, 34]
            };
            ensure!(
                required
                    .iter()
                    .all(|kind| headers.iter().any(|h| !h.is_empty()
                        && (if codec == VideoCodec::H264 {
                            h[0] & 31
                        } else {
                            (h[0] >> 1) & 63
                        }) == *kind)),
                "corrupt: first IDR lacks codec parameters"
            );
            let new_session = Session::detached(recording_origin, codec, directory.to_owned())?;
            writer = Some(RecordingState::for_import(
                new_session.clone(),
                &codec,
                recording_origin,
            )?);
            session = Some(new_session);
        }
        let pts = u64::try_from(pts).map_err(|_| anyhow!("corrupt: negative presentation time"))?;
        let dts = u64::try_from(dts).map_err(|_| anyhow!("corrupt: negative decode time"))?;
        ensure!(
            last_dts.is_none_or(|last| dts > last),
            "corrupt: nonmonotonic decode time"
        );
        ensure!(
            recording_origin
                .checked_mul(1_000_000)
                .and_then(|origin| origin.checked_add(pts.max(dts)))
                .is_some(),
            "corrupt: absolute timestamp overflow"
        );
        last_dts = Some(dts);
        let bytes = if key {
            let mut bytes = nal_utils::build_annex_b(&headers);
            bytes.extend_from_slice(&nal_utils::build_annex_b(&nals));
            bytes
        } else {
            bytes
        };
        writer.as_mut().unwrap().push_timed(&bytes, key, pts, dts)?;
        written += 1;
    }
    let mut writer = writer.ok_or_else(|| anyhow!("corrupt: no IDR frame"))?;
    writer.stop()?;
    session.unwrap().close();
    ensure!(!cancel.is_cancelled(), "cancelled");
    let mut verified = 0usize;
    for segment in segments::list_in(directory)? {
        ensure!(!cancel.is_cancelled(), "cancelled");
        let mut reader = super::reader::SegmentReader::open(segment)?;
        while reader.next()?.is_some() {
            ensure!(!cancel.is_cancelled(), "cancelled");
            verified += 1;
        }
    }
    ensure!(
        verified == written,
        "corrupt: normalized DVR lost video frames ({verified}/{written}, {codec:?})"
    );
    result_in(directory)?.ok_or_else(|| anyhow!("corrupt: no completed DVR segments"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use gstreamer::{self as gst, prelude::*};

    pub(crate) fn fixture(path: &Path, hevc: bool, bframes: bool) {
        gst::init().unwrap();
        let encoder = if hevc {
            "x265enc key-int-max=10 option-string=pools=1:frame-threads=1:open-gop=0"
        } else if bframes {
            "x264enc key-int-max=10 bframes=2"
        } else {
            "x264enc tune=zerolatency key-int-max=10"
        };
        let parser = if hevc { "h265parse" } else { "h264parse" };
        let output = if path
            .extension()
            .is_some_and(|ext| ext == "h264" || ext == "h265")
        {
            format!(
                "video/x-{},stream-format=byte-stream,alignment=au ! filesink location={}",
                if hevc { "h265" } else { "h264" },
                path.display()
            )
        } else {
            format!("mp4mux ! filesink location={}", path.display())
        };
        let pipeline = gst::parse::launch(&format!("videotestsrc num-buffers=125 ! video/x-raw,format=I420,width=160,height=96,framerate=10/1 ! {encoder} ! {parser} ! {output}")).unwrap().downcast::<gst::Pipeline>().unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let message = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(15),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            )
            .unwrap();
        pipeline.set_state(gst::State::Null).unwrap();
        assert_eq!(message.type_(), gst::MessageType::Eos);
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn generated_codecs_rotate_into_playable_segments_with_original_timing() {
        for (hevc, bframes) in [(false, false), (true, false), (false, true)] {
            let root = std::env::temp_dir().join(format!("ms-normalize-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let input = root.join("input.mp4");
            fixture(&input, hevc, bframes);
            let output = root.join("segments");
            let result = normalize(
                &input,
                &output,
                1_700_000_000_000,
                &CancellationToken::new(),
            )
            .unwrap();
            assert_eq!(result.codec, if hevc { "h265" } else { "h264" });
            assert_eq!(result.actual_start_ms, 1_700_000_000_000);
            assert!((12_300..=12_500).contains(&(result.actual_end_ms - result.actual_start_ms)));
            let files = segments::list_in(&output).unwrap();
            assert!(files.len() >= 2);
            for file in &files {
                file.validate().unwrap();
                assert!(file.path.with_extension("json").is_file());
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    #[tokio::test]
    async fn cancelled_import_does_not_publish_segments() {
        let root = std::env::temp_dir().join(format!("ms-normalize-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let input = root.join("input.mp4");
        fixture(&input, false, false);
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(normalize(&input, &root.join("output"), 1_700_000_000_000, &cancel).is_err());
        assert!(!root.join("output").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test(flavor = "multi_thread")]
    async fn elementary_streams_rebuild_missing_timestamps_from_their_frame_rate() {
        for hevc in [false, true] {
            let root = std::env::temp_dir().join(format!("ms-retime-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let input = root.join(if hevc { "input.h265" } else { "input.h264" });
            fixture(&input, hevc, false);
            let mut context = ffmpeg::format::input(&input).unwrap();
            let first = read_packet(&mut context).unwrap().unwrap();
            assert!(first.pts().is_none() || first.dts().is_none());
            let output = root.join("output");
            let result = normalize(
                &input,
                &output,
                1_700_000_000_000,
                &CancellationToken::new(),
            )
            .unwrap();
            assert_eq!(result.actual_start_ms, 1_700_000_000_000);
            assert_eq!(result.actual_end_ms, 1_700_000_012_400);
            let count: usize = segments::list_in(&output)
                .unwrap()
                .into_iter()
                .map(|s| super::super::reader::read_segment(s).unwrap().len())
                .sum();
            assert_eq!(count, 125);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    #[tokio::test]
    async fn non_video_and_unsupported_codec_are_reported_without_output() {
        let root = std::env::temp_dir().join(format!("ms-no-video-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let ppm = root.join("image.ppm");
        std::fs::write(&ppm, b"P6\n1 1\n255\n\x00\xff\x00").unwrap();
        let output = root.join("output");
        assert!(
            normalize(&ppm, &output, 1_700_000_000_000, &CancellationToken::new())
                .unwrap_err()
                .to_string()
                .starts_with("unsupported_codec:")
        );
        let wav = root.join("audio.wav");
        let mut wave = b"RIFF".to_vec();
        wave.extend_from_slice(&8036u32.to_le_bytes());
        wave.extend_from_slice(b"WAVEfmt ");
        wave.extend_from_slice(&16u32.to_le_bytes());
        wave.extend_from_slice(&1u16.to_le_bytes());
        wave.extend_from_slice(&1u16.to_le_bytes());
        wave.extend_from_slice(&8000u32.to_le_bytes());
        wave.extend_from_slice(&8000u32.to_le_bytes());
        wave.extend_from_slice(&1u16.to_le_bytes());
        wave.extend_from_slice(&8u16.to_le_bytes());
        wave.extend_from_slice(b"data");
        wave.extend_from_slice(&8000u32.to_le_bytes());
        wave.extend_from_slice(&vec![128; 8000]);
        std::fs::write(&wav, wave).unwrap();
        assert!(
            normalize(&wav, &output, 1_700_000_000_000, &CancellationToken::new())
                .unwrap_err()
                .to_string()
                .starts_with("no_footage:")
        );
        assert!(!output.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
