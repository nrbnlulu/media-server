use std::fmt;

pub mod nal_utils;
pub mod rtp;
pub mod traits;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    H265,
}

impl VideoCodec {
    pub fn from_encoding_name(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "h264" => Some(VideoCodec::H264),
            "h265" | "hevc" => Some(VideoCodec::H265),
            _ => None,
        }
    }

    pub fn mime_type(&self) -> &'static str {
        match self {
            VideoCodec::H264 => "video/H264",
            VideoCodec::H265 => "video/H265",
        }
    }
}

#[derive(Clone, Debug)]
pub struct TimeBase {
    pub num: i32,
    pub den: i32,
}

impl TimeBase {
    pub fn from_ffmpeg(rational: ffmpeg::Rational) -> anyhow::Result<Self> {
        let num = rational.numerator();
        let den = rational.denominator();
        if num <= 0 || den <= 0 {
            anyhow::bail!("Invalid timebase: {}/{}", num, den);
        } else {
            Ok(TimeBase { num, den })
        }
    }

    pub fn is_valid(&self) -> bool {
        self.num > 0 && self.den > 0
    }
}

#[derive(Clone)]
pub struct FFmpegVideoMetadata {
    pub codec: VideoCodec,
    pub extradata: Option<Vec<u8>>,
    pub parameters: ffmpeg::codec::Parameters,
    pub timebase: TimeBase,
}

impl fmt::Debug for FFmpegVideoMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FFmpegVideoMetadata")
            .field("codec", &self.codec)
            .field("extradata", &self.extradata)
            .field("timebase", &self.timebase)
            .finish()
    }
}
