//! Video frame packetizer.
//!
//! Splits encoded video frames into MTU-sized RTP packets, adds FEC parity
//! packets, and prepares them for UDP transmission.

use bytes::BytesMut;

use crate::fec::FecEncoder;
use crate::rtp::{FluxVideoExtension, RtpHeader};

/// Splits encoded video frames into network packets.
pub struct Packetizer {
    fec_encoder: FecEncoder,
    ssrc: u32,
}

impl Packetizer {
    pub fn new(fec_percentage: u8, ssrc: u32) -> Self {
        Self {
            fec_encoder: FecEncoder::new(fec_percentage),
            ssrc,
        }
    }

    /// Packetize an encoded video frame into RTP packets.
    ///
    /// Returns a list of packets (data + FEC) ready for UDP transmission.
    pub fn packetize(
        &self,
        frame_data: &[u8],
        is_keyframe: bool,
        frame_number: u32,
        sequence_number: &mut u16,
        rtp_timestamp: u32,
        max_packet_size: usize,
        payload_type: u8,
    ) -> Vec<Vec<u8>> {
        let header_overhead = RtpHeader::SIZE + FluxVideoExtension::SIZE;
        let max_payload = max_packet_size.saturating_sub(header_overhead);

        if max_payload == 0 {
            tracing::error!("max_packet_size too small to fit headers");
            return vec![];
        }

        let chunks: Vec<&[u8]> = frame_data.chunks(max_payload).collect();
        let total_data_packets = chunks.len() as u16;
        let fec_count = self.fec_encoder.parity_count(chunks.len());

        let mut packets = Vec::with_capacity(chunks.len() + fec_count);
        let mut data_payloads = Vec::with_capacity(chunks.len());

        for (index, chunk) in chunks.iter().enumerate() {
            let is_last = index == chunks.len() - 1;

            let rtp_header = RtpHeader {
                payload_type,
                sequence_number: *sequence_number,
                timestamp: rtp_timestamp,
                ssrc: self.ssrc,
                marker: is_last,
            };

            let video_ext = FluxVideoExtension {
                frame_number,
                total_data_packets,
                packet_index: index as u16,
                fec_packet_count: fec_count as u16,
                is_idr: is_keyframe,
            };

            let mut buf = BytesMut::with_capacity(header_overhead + chunk.len());
            rtp_header.serialize(&mut buf);
            video_ext.serialize(&mut buf);
            buf.extend_from_slice(chunk);

            data_payloads.push(chunk.to_vec());
            packets.push(buf.to_vec());
            *sequence_number = sequence_number.wrapping_add(1);
        }

        if let Ok(parity_packets) = self.fec_encoder.encode(&data_payloads) {
            for (index, parity) in parity_packets.iter().enumerate() {
                let rtp_header = RtpHeader {
                    payload_type: crate::rtp::payload_types::FEC,
                    sequence_number: *sequence_number,
                    timestamp: rtp_timestamp,
                    ssrc: self.ssrc,
                    marker: false,
                };

                let video_ext = FluxVideoExtension {
                    frame_number,
                    total_data_packets,
                    packet_index: (total_data_packets as usize + index) as u16,
                    fec_packet_count: fec_count as u16,
                    is_idr: is_keyframe,
                };

                let mut buf = BytesMut::with_capacity(header_overhead + parity.len());
                rtp_header.serialize(&mut buf);
                video_ext.serialize(&mut buf);
                buf.extend_from_slice(parity);

                packets.push(buf.to_vec());
                *sequence_number = sequence_number.wrapping_add(1);
            }
        }

        tracing::trace!(
            "Packetized frame {}: {} bytes -> {} data + {} FEC packets (keyframe={})",
            frame_number,
            frame_data.len(),
            total_data_packets,
            fec_count,
            is_keyframe,
        );

        packets
    }
}

/// Reassembles video frames from received RTP packets.
pub struct Depacketizer {
    current_frame: Option<FrameAssembly>,
}

struct FrameAssembly {
    frame_number: u32,
    total_data_packets: u16,
    is_idr: bool,
    received_packets: Vec<Option<Vec<u8>>>,
    received_count: usize,
}

/// A fully reassembled video frame from received packets.
#[derive(Debug, Clone)]
pub struct ReassembledFrame {
    pub frame_number: u32,
    pub is_idr: bool,
    pub data: Vec<u8>,
}

impl Depacketizer {
    pub fn new() -> Self {
        Self {
            current_frame: None,
        }
    }

    /// Feed a received RTP packet. Returns a complete frame if assembly is done.
    pub fn feed(&mut self, packet: &[u8]) -> Option<ReassembledFrame> {
        let header_size = RtpHeader::SIZE + FluxVideoExtension::SIZE;
        if packet.len() < header_size {
            return None;
        }

        let _rtp = RtpHeader::parse(packet)?;
        let ext = FluxVideoExtension::parse(&packet[RtpHeader::SIZE..])?;
        let payload = &packet[header_size..];

        let is_new_frame = match &self.current_frame {
            Some(frame) => frame.frame_number != ext.frame_number,
            None => true,
        };

        if is_new_frame {
            self.current_frame = Some(FrameAssembly {
                frame_number: ext.frame_number,
                total_data_packets: ext.total_data_packets,
                is_idr: ext.is_idr,
                received_packets: vec![None; ext.total_data_packets as usize],
                received_count: 0,
            });
        }

        let frame = self.current_frame.as_mut()?;
        let idx = ext.packet_index as usize;

        if idx < frame.received_packets.len() && frame.received_packets[idx].is_none() {
            frame.received_packets[idx] = Some(payload.to_vec());
            frame.received_count += 1;
        }

        if frame.received_count == frame.total_data_packets as usize {
            let assembled: Vec<u8> = frame
                .received_packets
                .iter()
                .filter_map(|p| p.as_ref())
                .flat_map(|p| p.iter().copied())
                .collect();

            let result = ReassembledFrame {
                frame_number: frame.frame_number,
                is_idr: frame.is_idr,
                data: assembled,
            };

            self.current_frame = None;
            return Some(result);
        }

        None
    }
}
