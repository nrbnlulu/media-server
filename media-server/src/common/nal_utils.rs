//! NAL (Network Abstraction Layer) unit utilities for H.264 and H.265 video codecs.
//!
//! This module provides common functionality for parsing, identifying, and converting
//! NAL units between different framing formats (Annex-B and AVCC/HVCC).

use crate::common::VideoCodec;

/// H.264 NAL unit types as defined in ITU-T H.264 specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum H264NalType {
    /// Unspecified
    Unspecified = 0,
    /// Coded slice of a non-IDR picture
    NonIdrSlice = 1,
    /// Coded slice data partition A
    SliceDataA = 2,
    /// Coded slice data partition B
    SliceDataB = 3,
    /// Coded slice data partition C
    SliceDataC = 4,
    /// Coded slice of an IDR picture (keyframe)
    IdrSlice = 5,
    /// Supplemental enhancement information (SEI)
    Sei = 6,
    /// Sequence parameter set (SPS)
    Sps = 7,
    /// Picture parameter set (PPS)
    Pps = 8,
    /// Access unit delimiter
    AccessUnitDelimiter = 9,
    /// End of sequence
    EndOfSequence = 10,
    /// End of stream
    EndOfStream = 11,
    /// Filler data
    FillerData = 12,
    /// SPS extension
    SpsExtension = 13,
    /// Prefix NAL unit
    PrefixNalUnit = 14,
    /// Subset SPS
    SubsetSps = 15,
    /// Reserved 16-18
    Reserved16 = 16,
    Reserved17 = 17,
    Reserved18 = 18,
    /// Coded slice of an auxiliary coded picture without partitioning
    AuxSlice = 19,
    /// Coded slice extension
    SliceExtension = 20,
    /// Reserved 21-23
    Reserved21 = 21,
    Reserved22 = 22,
    Reserved23 = 23,
}

impl H264NalType {
    /// Extracts NAL type from the first byte of a NAL unit.
    pub fn from_byte(byte: u8) -> Option<Self> {
        let nal_type = byte & 0x1F;
        match nal_type {
            0 => Some(Self::Unspecified),
            1 => Some(Self::NonIdrSlice),
            2 => Some(Self::SliceDataA),
            3 => Some(Self::SliceDataB),
            4 => Some(Self::SliceDataC),
            5 => Some(Self::IdrSlice),
            6 => Some(Self::Sei),
            7 => Some(Self::Sps),
            8 => Some(Self::Pps),
            9 => Some(Self::AccessUnitDelimiter),
            10 => Some(Self::EndOfSequence),
            11 => Some(Self::EndOfStream),
            12 => Some(Self::FillerData),
            13 => Some(Self::SpsExtension),
            14 => Some(Self::PrefixNalUnit),
            15 => Some(Self::SubsetSps),
            16 => Some(Self::Reserved16),
            17 => Some(Self::Reserved17),
            18 => Some(Self::Reserved18),
            19 => Some(Self::AuxSlice),
            20 => Some(Self::SliceExtension),
            21 => Some(Self::Reserved21),
            22 => Some(Self::Reserved22),
            23 => Some(Self::Reserved23),
            _ => None,
        }
    }

    /// Returns the raw NAL type value.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Checks if this NAL unit is a keyframe (IDR slice).
    pub fn is_keyframe(self) -> bool {
        matches!(self, Self::IdrSlice)
    }

    /// Checks if this NAL unit is a parameter set (SPS or PPS).
    pub fn is_parameter_set(self) -> bool {
        matches!(self, Self::Sps | Self::Pps)
    }
}

/// H.265/HEVC NAL unit types as defined in ITU-T H.265 specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum H265NalType {
    /// Coded slice segment of a non-TSA, non-STSA trailing picture
    TrailN = 0,
    TrailR = 1,
    /// Coded slice segment of a TSA picture
    TsaN = 2,
    TsaR = 3,
    /// Coded slice segment of an STSA picture
    StsaN = 4,
    StsaR = 5,
    /// Coded slice segment of a RADL picture
    RadlN = 6,
    RadlR = 7,
    /// Coded slice segment of a RASL picture
    RaslN = 8,
    RaslR = 9,
    /// Reserved
    Reserved10 = 10,
    Reserved11 = 11,
    Reserved12 = 12,
    Reserved13 = 13,
    Reserved14 = 14,
    Reserved15 = 15,
    /// Coded slice segment of a BLA picture (keyframe)
    BlaNLp = 16,
    BlaWLp = 17,
    BlaWRadl = 18,
    /// Coded slice segment of an IDR picture (keyframe)
    IdrWRadl = 19,
    IdrNLp = 20,
    /// Coded slice segment of a CRA picture (keyframe)
    CraNut = 21,
    /// Reserved
    Reserved22 = 22,
    Reserved23 = 23,
    Reserved24 = 24,
    Reserved25 = 25,
    Reserved26 = 26,
    Reserved27 = 27,
    Reserved28 = 28,
    Reserved29 = 29,
    Reserved30 = 30,
    Reserved31 = 31,
    /// Video parameter set (VPS)
    Vps = 32,
    /// Sequence parameter set (SPS)
    Sps = 33,
    /// Picture parameter set (PPS)
    Pps = 34,
    /// Access unit delimiter
    AccessUnitDelimiter = 35,
    /// End of sequence
    EndOfSequence = 36,
    /// End of bitstream
    EndOfBitstream = 37,
    /// Filler data
    FillerData = 38,
    /// Supplemental enhancement information (SEI)
    SeiPrefix = 39,
    SeiSuffix = 40,
}

impl H265NalType {
    /// Extracts NAL type from the first two bytes of a NAL unit.
    pub fn from_header(byte1: u8, byte2: u8) -> Option<Self> {
        let nal_type = (byte1 >> 1) & 0x3F;
        match nal_type {
            0 => Some(Self::TrailN),
            1 => Some(Self::TrailR),
            2 => Some(Self::TsaN),
            3 => Some(Self::TsaR),
            4 => Some(Self::StsaN),
            5 => Some(Self::StsaR),
            6 => Some(Self::RadlN),
            7 => Some(Self::RadlR),
            8 => Some(Self::RaslN),
            9 => Some(Self::RaslR),
            10 => Some(Self::Reserved10),
            11 => Some(Self::Reserved11),
            12 => Some(Self::Reserved12),
            13 => Some(Self::Reserved13),
            14 => Some(Self::Reserved14),
            15 => Some(Self::Reserved15),
            16 => Some(Self::BlaNLp),
            17 => Some(Self::BlaWLp),
            18 => Some(Self::BlaWRadl),
            19 => Some(Self::IdrWRadl),
            20 => Some(Self::IdrNLp),
            21 => Some(Self::CraNut),
            22 => Some(Self::Reserved22),
            23 => Some(Self::Reserved23),
            24 => Some(Self::Reserved24),
            25 => Some(Self::Reserved25),
            26 => Some(Self::Reserved26),
            27 => Some(Self::Reserved27),
            28 => Some(Self::Reserved28),
            29 => Some(Self::Reserved29),
            30 => Some(Self::Reserved30),
            31 => Some(Self::Reserved31),
            32 => Some(Self::Vps),
            33 => Some(Self::Sps),
            34 => Some(Self::Pps),
            35 => Some(Self::AccessUnitDelimiter),
            36 => Some(Self::EndOfSequence),
            37 => Some(Self::EndOfBitstream),
            38 => Some(Self::FillerData),
            39 => Some(Self::SeiPrefix),
            40 => Some(Self::SeiSuffix),
            _ => None,
        }
    }

    /// Returns the raw NAL type value.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Checks if this NAL unit is a keyframe (IDR, BLA, or CRA slice).
    pub fn is_keyframe(self) -> bool {
        matches!(
            self,
            Self::BlaNLp
                | Self::BlaWLp
                | Self::BlaWRadl
                | Self::IdrWRadl
                | Self::IdrNLp
                | Self::CraNut
        )
    }

    /// Checks if this NAL unit is a parameter set (VPS, SPS, or PPS).
    pub fn is_parameter_set(self) -> bool {
        matches!(self, Self::Vps | Self::Sps | Self::Pps)
    }
}

/// Checks if an H.264 NAL unit is a keyframe (IDR slice).
pub fn is_h264_keyframe(nal: &[u8]) -> bool {
    if nal.is_empty() {
        return false;
    }
    H264NalType::from_byte(nal[0])
        .map(|t| t.is_keyframe())
        .unwrap_or(false)
}

/// Checks if an H.265 NAL unit is a keyframe (IDR, BLA, or CRA slice).
pub fn is_h265_keyframe(nal: &[u8]) -> bool {
    if nal.len() < 2 {
        return false;
    }
    H265NalType::from_header(nal[0], nal[1])
        .map(|t| t.is_keyframe())
        .unwrap_or(false)
}

/// Checks if an H.264 NAL unit is a parameter set (SPS or PPS).
pub fn is_h264_parameter_set(nal: &[u8]) -> bool {
    if nal.is_empty() {
        return false;
    }
    H264NalType::from_byte(nal[0])
        .map(|t| t.is_parameter_set())
        .unwrap_or(false)
}

/// Checks if an H.265 NAL unit is a parameter set (VPS, SPS, or PPS).
pub fn is_h265_parameter_set(nal: &[u8]) -> bool {
    if nal.len() < 2 {
        return false;
    }
    H265NalType::from_header(nal[0], nal[1])
        .map(|t| t.is_parameter_set())
        .unwrap_or(false)
}

/// Checks if a NAL unit is a keyframe for the given codec.
pub fn is_keyframe(nal: &[u8], codec: &VideoCodec) -> bool {
    match codec {
        VideoCodec::H264 => is_h264_keyframe(nal),
        VideoCodec::H265 => is_h265_keyframe(nal),
    }
}

/// Checks if a NAL unit is a parameter set for the given codec.
pub fn is_parameter_set(nal: &[u8], codec: &VideoCodec) -> bool {
    match codec {
        VideoCodec::H264 => is_h264_parameter_set(nal),
        VideoCodec::H265 => is_h265_parameter_set(nal),
    }
}

/// Gets the H.264 NAL type from a NAL unit.
pub fn get_h264_nal_type(nal: &[u8]) -> Option<H264NalType> {
    if nal.is_empty() {
        return None;
    }
    H264NalType::from_byte(nal[0])
}

/// Gets the H.265 NAL type from a NAL unit.
pub fn get_h265_nal_type(nal: &[u8]) -> Option<H265NalType> {
    if nal.len() < 2 {
        return None;
    }
    H265NalType::from_header(nal[0], nal[1])
}

/// Framing format for video NAL units.
#[derive(Debug, Clone, PartialEq)]
pub enum FramingFormat {
    /// Used for MP4/MKV/MOV and AVCC/HVCC streams.
    /// The length_size is the number of bytes used for the NAL unit length prefix.
    Avcc { length_size: usize },
    /// Used for MPEG-TS, RTSP, and raw bitstreams using start codes (00 00 01).
    AnnexB,
}

/// Parsed extradata containing NAL units and framing format information.
pub struct ParsedExtraData {
    /// The NAL units extracted from the configuration record (VPS, SPS, PPS).
    pub nals: Vec<Vec<u8>>,
    /// The framing format used for the subsequent video packets in this stream.
    pub framing_format: FramingFormat,
}

/// Checks if data is in Annex-B format (starts with start code).
pub fn is_annex_b(data: &[u8]) -> bool {
    if data.len() < 3 {
        return false;
    }
    (data[0] == 0 && data[1] == 0 && data[2] == 1)
        || (data.len() >= 4 && data[0] == 0 && data[1] == 0 && data[2] == 0 && data[3] == 1)
}

/// Parses NAL units from Annex-B formatted data (with start codes).
pub fn parse_annex_b(data: &[u8]) -> Vec<Vec<u8>> {
    let mut nal_units = Vec::new();
    let mut start_indices = Vec::new();

    let mut i = 0;
    while i < data.len() {
        if i + 3 <= data.len() && data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                start_indices.push(i + 3);
                i += 3;
                continue;
            } else if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                start_indices.push(i + 4);
                i += 4;
                continue;
            }
        }
        i += 1;
    }

    for (idx, &start) in start_indices.iter().enumerate() {
        let end = if idx + 1 < start_indices.len() {
            let next_start = start_indices[idx + 1];
            if next_start >= 4 && data[next_start - 4] == 0 {
                next_start - 4
            } else {
                next_start - 3
            }
        } else {
            data.len()
        };

        if start < end {
            nal_units.push(data[start..end].to_vec());
        }
    }

    nal_units
}

/// Parses NAL units from AVCC/HVCC formatted data (with length prefixes).
pub fn parse_avcc(data: &[u8], length_size: &usize) -> Vec<Vec<u8>> {
    let mut nal_units = Vec::new();
    let mut offset = 0;
    while offset + length_size <= data.len() {
        let nal_length = match length_size {
            4 => u32::from_be_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize,
            2 => u16::from_be_bytes([data[offset], data[offset + 1]]) as usize,
            1 => data[offset] as usize,
            _ => break,
        };

        offset += length_size;

        if offset + nal_length > data.len() {
            break;
        }

        nal_units.push(data[offset..offset + nal_length].to_vec());
        offset += nal_length;
    }

    nal_units
}

/// Parses NAL units based on the framing format.
pub fn parse_nal_units_with_length(data: &[u8], framing_fmt: &FramingFormat) -> Vec<Vec<u8>> {
    match framing_fmt {
        FramingFormat::AnnexB => parse_annex_b(data),
        FramingFormat::Avcc { length_size } => parse_avcc(data, length_size),
    }
}

/// Builds Annex-B formatted data from NAL units (adds start codes).
pub fn build_annex_b(nal_units: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nal_units {
        if nal.is_empty() {
            continue;
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(nal);
    }
    out
}

/// Parses H.264 extradata (supports both AVCC and Annex-B formats).
pub fn parse_h264_extradata(extradata: &[u8]) -> Option<ParsedExtraData> {
    // Try Annex-B format first (common from RTSP/FFmpeg)
    if is_annex_b(extradata) {
        let nals = parse_annex_b(extradata);
        // Filter to only include SPS and PPS
        let param_nals: Vec<Vec<u8>> = nals
            .into_iter()
            .filter(|nal| {
                if nal.is_empty() {
                    return false;
                }
                let nal_type = nal[0] & 0x1F;
                // SPS = 7, PPS = 8
                nal_type == 7 || nal_type == 8
            })
            .collect();

        if !param_nals.is_empty() {
            return Some(ParsedExtraData {
                nals: param_nals,
                // Annex-B extradata typically means Annex-B framing for packets too,
                // but FFmpeg often uses AVCC framing for actual packets
                framing_format: FramingFormat::Avcc { length_size: 4 },
            });
        }
    }

    // Try AVCC format (AVCDecoderConfigurationRecord)
    if extradata.len() < 7 || extradata[0] != 1 {
        return None;
    }

    let length_size = ((extradata[4] & 0x03) + 1) as usize;
    let num_sps = (extradata[5] & 0x1F) as usize;
    let mut offset = 6;
    let mut nals = Vec::new();

    for _ in 0..num_sps {
        if offset + 2 > extradata.len() {
            return None;
        }
        let len = u16::from_be_bytes([extradata[offset], extradata[offset + 1]]) as usize;
        offset += 2;
        if offset + len > extradata.len() {
            return None;
        }
        nals.push(extradata[offset..offset + len].to_vec());
        offset += len;
    }

    if offset < extradata.len() {
        let num_pps = extradata[offset] as usize;
        offset += 1;

        for _ in 0..num_pps {
            if offset + 2 > extradata.len() {
                return None;
            }
            let len = u16::from_be_bytes([extradata[offset], extradata[offset + 1]]) as usize;
            offset += 2;
            if offset + len > extradata.len() {
                return None;
            }
            nals.push(extradata[offset..offset + len].to_vec());
            offset += len;
        }
    }

    Some(ParsedExtraData {
        nals,
        framing_format: FramingFormat::Avcc { length_size },
    })
}

/// Parses H.265 extradata (supports both HVCC and Annex-B formats).
pub fn parse_h265_extradata(extradata: &[u8]) -> Option<ParsedExtraData> {
    // Try Annex-B format first (common from RTSP/FFmpeg)
    if is_annex_b(extradata) {
        let nals = parse_annex_b(extradata);
        // Filter to only include VPS, SPS, and PPS
        let param_nals: Vec<Vec<u8>> = nals
            .into_iter()
            .filter(|nal| {
                if nal.len() < 2 {
                    return false;
                }
                let nal_type = (nal[0] >> 1) & 0x3F;
                // VPS = 32, SPS = 33, PPS = 34
                nal_type == 32 || nal_type == 33 || nal_type == 34
            })
            .collect();

        if !param_nals.is_empty() {
            return Some(ParsedExtraData {
                nals: param_nals,
                framing_format: FramingFormat::Avcc { length_size: 4 },
            });
        }
    }

    // Try HVCC format (HEVCDecoderConfigurationRecord)
    if extradata.len() < 23 || extradata[0] != 1 {
        return None;
    }

    let length_size = ((extradata[21] & 0x03) + 1) as usize;
    let num_arrays = extradata[22] as usize;
    let mut offset = 23;
    let mut nals = Vec::new();

    for _ in 0..num_arrays {
        if offset + 3 > extradata.len() {
            return None;
        }
        let nal_type = extradata[offset] & 0x3F;
        let num_nalus = u16::from_be_bytes([extradata[offset + 1], extradata[offset + 2]]) as usize;
        offset += 3;

        for _ in 0..num_nalus {
            if offset + 2 > extradata.len() {
                return None;
            }
            let len = u16::from_be_bytes([extradata[offset], extradata[offset + 1]]) as usize;
            offset += 2;
            if offset + len > extradata.len() {
                return None;
            }

            if matches!(nal_type, 32 | 33 | 34) {
                nals.push(extradata[offset..offset + len].to_vec());
            }
            offset += len;
        }
    }

    Some(ParsedExtraData {
        nals,
        framing_format: FramingFormat::Avcc { length_size },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_h264_nal_type_extraction() {
        let idr_byte = 0x65; // Type 5 (IDR) with NRI = 3
        let nal_type = H264NalType::from_byte(idr_byte).unwrap();
        assert_eq!(nal_type, H264NalType::IdrSlice);
        assert!(nal_type.is_keyframe());
        assert!(!nal_type.is_parameter_set());
    }

    #[test]
    fn test_h264_parameter_sets() {
        let sps_byte = 0x67; // Type 7 (SPS)
        let pps_byte = 0x68; // Type 8 (PPS)

        let sps_type = H264NalType::from_byte(sps_byte).unwrap();
        let pps_type = H264NalType::from_byte(pps_byte).unwrap();

        assert_eq!(sps_type, H264NalType::Sps);
        assert_eq!(pps_type, H264NalType::Pps);
        assert!(sps_type.is_parameter_set());
        assert!(pps_type.is_parameter_set());
    }

    #[test]
    fn test_h265_nal_type_extraction() {
        let idr_header = [0x26, 0x01]; // Type 19 (IDR_W_RADL)
        let nal_type = H265NalType::from_header(idr_header[0], idr_header[1]).unwrap();
        assert_eq!(nal_type, H265NalType::IdrWRadl);
        assert!(nal_type.is_keyframe());
        assert!(!nal_type.is_parameter_set());
    }

    #[test]
    fn test_h265_parameter_sets() {
        let vps_header = [0x40, 0x01]; // Type 32 (VPS)
        let sps_header = [0x42, 0x01]; // Type 33 (SPS)
        let pps_header = [0x44, 0x01]; // Type 34 (PPS)

        let vps_type = H265NalType::from_header(vps_header[0], vps_header[1]).unwrap();
        let sps_type = H265NalType::from_header(sps_header[0], sps_header[1]).unwrap();
        let pps_type = H265NalType::from_header(pps_header[0], pps_header[1]).unwrap();

        assert_eq!(vps_type, H265NalType::Vps);
        assert_eq!(sps_type, H265NalType::Sps);
        assert_eq!(pps_type, H265NalType::Pps);
        assert!(vps_type.is_parameter_set());
        assert!(sps_type.is_parameter_set());
        assert!(pps_type.is_parameter_set());
    }

    #[test]
    fn test_parse_annex_b() {
        let data = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1e, 0x00, 0x00, 0x00, 0x01, 0x68, 0xce,
            0x3c, 0x80,
        ];

        let nals = parse_annex_b(&data);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], vec![0x67, 0x42, 0x00, 0x1e]);
        assert_eq!(nals[1], vec![0x68, 0xce, 0x3c, 0x80]);
    }

    #[test]
    fn test_parse_avcc() {
        let data = vec![
            0x00, 0x00, 0x00, 0x04, 0x67, 0x42, 0x00, 0x1e, 0x00, 0x00, 0x00, 0x04, 0x68, 0xce,
            0x3c, 0x80,
        ];

        let nals = parse_avcc(&data, &4);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], vec![0x67, 0x42, 0x00, 0x1e]);
        assert_eq!(nals[1], vec![0x68, 0xce, 0x3c, 0x80]);
    }

    #[test]
    fn test_parse_avcc_length_size_two() {
        let data = vec![
            0x00, 0x04, 0x67, 0x42, 0x00, 0x1e, 0x00, 0x04, 0x68, 0xce, 0x3c, 0x80,
        ];

        let nals = parse_avcc(&data, &2);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], vec![0x67, 0x42, 0x00, 0x1e]);
        assert_eq!(nals[1], vec![0x68, 0xce, 0x3c, 0x80]);
    }

    #[test]
    fn test_build_annex_b() {
        let nals = vec![vec![0x67, 0x42, 0x00, 0x1e], vec![0x68, 0xce, 0x3c, 0x80]];

        let data = build_annex_b(&nals);
        assert_eq!(
            data,
            vec![
                0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0x00, 0x1e, 0x00, 0x00, 0x00, 0x01, 0x68, 0xce,
                0x3c, 0x80
            ]
        );
    }

    #[test]
    fn test_is_annex_b() {
        let annex_b_3 = vec![0x00, 0x00, 0x01, 0x67];
        let annex_b_4 = vec![0x00, 0x00, 0x00, 0x01, 0x67];
        let avcc = vec![0x00, 0x00, 0x00, 0x04, 0x67];

        assert!(is_annex_b(&annex_b_3));
        assert!(is_annex_b(&annex_b_4));
        assert!(!is_annex_b(&avcc));
    }

    #[test]
    fn test_is_h264_keyframe() {
        let idr_nal = vec![0x65, 0x88, 0x84];
        let non_idr_nal = vec![0x41, 0x9a, 0x00];

        assert!(is_h264_keyframe(&idr_nal));
        assert!(!is_h264_keyframe(&non_idr_nal));
    }

    #[test]
    fn test_is_h265_keyframe() {
        let idr_nal = vec![0x26, 0x01, 0xaf];
        let non_idr_nal = vec![0x02, 0x01, 0xd0];

        assert!(is_h265_keyframe(&idr_nal));
        assert!(!is_h265_keyframe(&non_idr_nal));
    }

    #[test]
    fn test_is_h264_parameter_set() {
        let sps_nal = vec![0x67, 0x42, 0x00, 0x1e];
        let pps_nal = vec![0x68, 0xce, 0x3c, 0x80];
        let idr_nal = vec![0x65, 0x88, 0x84];

        assert!(is_h264_parameter_set(&sps_nal));
        assert!(is_h264_parameter_set(&pps_nal));
        assert!(!is_h264_parameter_set(&idr_nal));
    }

    #[test]
    fn test_is_h265_parameter_set() {
        let vps_nal = vec![0x40, 0x01, 0x0c];
        let sps_nal = vec![0x42, 0x01, 0x01, 0x60];
        let pps_nal = vec![0x44, 0x01, 0xc0];
        let idr_nal = vec![0x26, 0x01, 0xaf];

        assert!(is_h265_parameter_set(&vps_nal));
        assert!(is_h265_parameter_set(&sps_nal));
        assert!(is_h265_parameter_set(&pps_nal));
        assert!(!is_h265_parameter_set(&idr_nal));
    }

    #[test]
    fn test_parse_h264_extradata() {
        let extradata = vec![
            0x01, 0x64, 0x00, 0x1e, 0xff, 0xe1, 0x00, 0x04, 0x67, 0x42, 0x00, 0x1e, 0x01, 0x00,
            0x04, 0x68, 0xce, 0x3c, 0x80,
        ];

        let parsed = parse_h264_extradata(&extradata).unwrap();
        assert_eq!(
            parsed.framing_format,
            FramingFormat::Avcc { length_size: 4 }
        );
        assert_eq!(parsed.nals.len(), 2);
        assert_eq!(parsed.nals[0], vec![0x67, 0x42, 0x00, 0x1e]);
        assert_eq!(parsed.nals[1], vec![0x68, 0xce, 0x3c, 0x80]);
    }

    #[test]
    fn test_parse_h265_extradata() {
        let vps = vec![0x40, 0x01, 0x0c];
        let sps = vec![0x42, 0x01, 0x01, 0x60];
        let pps = vec![0x44, 0x01, 0xc0];

        let mut extradata = vec![
            0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1e, 0xf0,
            0x00, 0xfc, 0xfc, 0xf8, 0xf8, 0x00, 0x00, 0x03, 0x03,
        ];

        // Helper to push H.265 array
        fn push_h265_array(buf: &mut Vec<u8>, nal_type: u8, nal: &[u8]) {
            buf.push(0x80 | (nal_type & 0x3f));
            buf.extend_from_slice(&1u16.to_be_bytes());
            buf.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            buf.extend_from_slice(nal);
        }

        push_h265_array(&mut extradata, 32, &vps);
        push_h265_array(&mut extradata, 33, &sps);
        push_h265_array(&mut extradata, 34, &pps);

        let parsed = parse_h265_extradata(&extradata).unwrap();
        assert_eq!(
            parsed.framing_format,
            FramingFormat::Avcc { length_size: 4 }
        );
        assert_eq!(parsed.nals.len(), 3);
        assert_eq!(parsed.nals[0], vps);
        assert_eq!(parsed.nals[1], sps);
        assert_eq!(parsed.nals[2], pps);
    }
}
