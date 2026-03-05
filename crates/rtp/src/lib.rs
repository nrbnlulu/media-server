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

    /// Check if packet is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
