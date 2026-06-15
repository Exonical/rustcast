//! RTP (Real-time Transport Protocol) header construction and parsing.
//!
//! Implements RFC 3550 RTP headers for video and audio packet framing.
//! Used over UDP for low-latency media delivery.

use bytes::{BufMut, BytesMut};

/// RTP protocol version (always 2).
const RTP_VERSION: u8 = 2;

/// RTP header (12 bytes fixed, plus optional CSRC and extensions).
#[derive(Debug, Clone)]
pub struct RtpHeader {
    /// Payload type (7 bits).
    pub payload_type: u8,

    /// Sequence number (16 bits, wraps at 65535).
    pub sequence_number: u16,

    /// Timestamp in media clock units (e.g. 90kHz for video).
    pub timestamp: u32,

    /// Synchronization source identifier.
    pub ssrc: u32,

    /// Whether this packet contains the start of a frame.
    pub marker: bool,
}

impl RtpHeader {
    /// Fixed header size in bytes.
    pub const SIZE: usize = 12;

    /// Serialize the RTP header into a byte buffer.
    pub fn serialize(&self, buf: &mut BytesMut) {
        // Byte 0: V=2, P=0, X=0, CC=0
        let byte0 = (RTP_VERSION << 6) & 0xC0;
        buf.put_u8(byte0);

        // Byte 1: M + PT
        let byte1 = if self.marker { 0x80 } else { 0x00 } | (self.payload_type & 0x7F);
        buf.put_u8(byte1);

        // Bytes 2-3: Sequence number
        buf.put_u16(self.sequence_number);

        // Bytes 4-7: Timestamp
        buf.put_u32(self.timestamp);

        // Bytes 8-11: SSRC
        buf.put_u32(self.ssrc);
    }

    /// Parse an RTP header from a byte slice. Returns `None` if the slice is too short.
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < Self::SIZE {
            return None;
        }

        let version = (data[0] >> 6) & 0x03;
        if version != RTP_VERSION {
            return None;
        }

        let marker = (data[1] & 0x80) != 0;
        let payload_type = data[1] & 0x7F;
        let sequence_number = u16::from_be_bytes([data[2], data[3]]);
        let timestamp = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

        Some(Self {
            payload_type,
            sequence_number,
            timestamp,
            ssrc,
            marker,
        })
    }
}

/// Well-known RTP payload types for Flux streams.
pub mod payload_types {
    /// H.264 video.
    pub const H264: u8 = 96;
    /// H.265/HEVC video.
    pub const H265: u8 = 97;
    /// AV1 video.
    pub const AV1: u8 = 98;
    /// Opus audio.
    pub const OPUS: u8 = 111;
    /// FEC repair packets.
    pub const FEC: u8 = 127;
}

/// Video-specific RTP extension header used by Flux.
///
/// Appended after the standard RTP header to carry frame metadata.
#[derive(Debug, Clone)]
pub struct FluxVideoExtension {
    /// Frame number (monotonically increasing per stream).
    pub frame_number: u32,

    /// Total number of data packets in this frame.
    pub total_data_packets: u16,

    /// Index of this packet within the frame (0-based).
    pub packet_index: u16,

    /// Total number of FEC packets for this frame.
    pub fec_packet_count: u16,

    /// Whether this frame is an IDR / keyframe.
    pub is_idr: bool,
}

impl FluxVideoExtension {
    pub const SIZE: usize = 12;

    pub fn serialize(&self, buf: &mut BytesMut) {
        buf.put_u32(self.frame_number);
        buf.put_u16(self.total_data_packets);
        buf.put_u16(self.packet_index);
        buf.put_u16(self.fec_packet_count);
        let flags = if self.is_idr { 0x01u16 } else { 0x00 };
        buf.put_u16(flags);
    }

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < Self::SIZE {
            return None;
        }
        let frame_number = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let total_data_packets = u16::from_be_bytes([data[4], data[5]]);
        let packet_index = u16::from_be_bytes([data[6], data[7]]);
        let fec_packet_count = u16::from_be_bytes([data[8], data[9]]);
        let flags = u16::from_be_bytes([data[10], data[11]]);
        let is_idr = (flags & 0x01) != 0;

        Some(Self {
            frame_number,
            total_data_packets,
            packet_index,
            fec_packet_count,
            is_idr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_header_round_trip() {
        let header = RtpHeader {
            payload_type: payload_types::H265,
            sequence_number: 12345,
            timestamp: 9_000_000,
            ssrc: 0xDEADBEEF,
            marker: true,
        };

        let mut buf = BytesMut::with_capacity(RtpHeader::SIZE);
        header.serialize(&mut buf);

        let parsed = RtpHeader::parse(&buf).expect("failed to parse");
        assert_eq!(parsed.payload_type, header.payload_type);
        assert_eq!(parsed.sequence_number, header.sequence_number);
        assert_eq!(parsed.timestamp, header.timestamp);
        assert_eq!(parsed.ssrc, header.ssrc);
        assert_eq!(parsed.marker, header.marker);
    }

    #[test]
    fn video_extension_round_trip() {
        let ext = FluxVideoExtension {
            frame_number: 42,
            total_data_packets: 10,
            packet_index: 3,
            fec_packet_count: 2,
            is_idr: true,
        };

        let mut buf = BytesMut::with_capacity(FluxVideoExtension::SIZE);
        ext.serialize(&mut buf);

        let parsed = FluxVideoExtension::parse(&buf).expect("failed to parse");
        assert_eq!(parsed.frame_number, 42);
        assert_eq!(parsed.total_data_packets, 10);
        assert_eq!(parsed.packet_index, 3);
        assert_eq!(parsed.fec_packet_count, 2);
        assert!(parsed.is_idr);
    }
}
