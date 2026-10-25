//! Forward Error Correction (FEC) for lossy UDP transport.
//!
//! Status: placeholder. The encoder produces at most one XOR parity packet
//! per group of data packets, whatever the configured overhead percentage.
//! There is no decoder yet, so lost packets cannot be recovered.
//!
//! TODO: replace the XOR parity with Reed-Solomon erasure coding (e.g. the
//! `reed-solomon-erasure` crate), raise [`MAX_PARITY_PACKETS`], and add a
//! decoder that reconstructs lost data packets.

/// Most parity packets [`FecEncoder::encode`] can produce per group.
///
/// Single XOR parity cannot produce more than one independent parity packet.
pub const MAX_PARITY_PACKETS: usize = 1;

/// FEC encoder: generates parity packets from a group of data packets.
pub struct FecEncoder {
    /// Requested parity overhead as a percentage of data packets (0–100).
    fec_percentage: u8,
}

impl FecEncoder {
    pub fn new(fec_percentage: u8) -> Self {
        Self {
            fec_percentage: fec_percentage.min(100),
        }
    }

    /// Number of parity packets [`encode`](Self::encode) produces for a group
    /// of `data_packet_count` data packets.
    ///
    /// The requested overhead is rounded up to at least one packet and then
    /// capped at [`MAX_PARITY_PACKETS`].
    pub fn parity_count(&self, data_packet_count: usize) -> usize {
        if data_packet_count == 0 {
            return 0;
        }
        let requested = (data_packet_count * self.fec_percentage as usize).div_ceil(100);
        requested.clamp(1, MAX_PARITY_PACKETS)
    }

    /// Generate FEC parity packets for a group of data packets.
    ///
    /// Shorter packets are treated as zero-padded to the longest one. Returns
    /// [`parity_count`](Self::parity_count) packets (a single XOR of all data
    /// packets), or none if every data packet is empty.
    pub fn encode(&self, data_packets: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let packet_size = data_packets.iter().map(|p| p.len()).max().unwrap_or(0);
        if packet_size == 0 {
            return vec![];
        }

        tracing::trace!(
            "FEC encode: {} data packets → {} XOR parity packet(s) ({}% requested)",
            data_packets.len(),
            self.parity_count(data_packets.len()),
            self.fec_percentage,
        );

        let mut parity = vec![0u8; packet_size];
        for packet in data_packets {
            for (i, byte) in packet.iter().enumerate() {
                parity[i] ^= byte;
            }
        }

        vec![parity]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parity_count_calculation() {
        let encoder = FecEncoder::new(20);
        assert_eq!(encoder.parity_count(0), 0);
        assert_eq!(encoder.parity_count(1), 1); // Always at least 1
        assert_eq!(encoder.parity_count(5), 1); // 20% of 5 = 1
        assert_eq!(encoder.parity_count(10), MAX_PARITY_PACKETS); // 20% of 10 = 2, capped
        assert_eq!(FecEncoder::new(0).parity_count(10), 1);
    }

    #[test]
    fn xor_parity_basic() {
        let encoder = FecEncoder::new(50);
        let data = vec![vec![0xAA, 0xBB], vec![0xCC, 0xDD]];
        // XOR parity: 0xAA ^ 0xCC = 0x66, 0xBB ^ 0xDD = 0x66
        assert_eq!(encoder.encode(&data), vec![vec![0x66, 0x66]]);
    }

    #[test]
    fn xor_parity_zero_pads_short_packets() {
        let encoder = FecEncoder::new(50);
        let data = vec![vec![0xAA, 0xBB], vec![0xCC]];
        assert_eq!(encoder.encode(&data), vec![vec![0x66, 0xBB]]);
    }

    #[test]
    fn encode_matches_parity_count_for_large_groups() {
        let encoder = FecEncoder::new(20);
        let data = vec![vec![0x01; 4]; 10];
        let parity = encoder.encode(&data);
        assert_eq!(parity.len(), encoder.parity_count(data.len()));
        assert_eq!(parity, vec![vec![0x00; 4]]);
    }

    #[test]
    fn encode_empty_group_has_no_parity() {
        let encoder = FecEncoder::new(20);
        assert!(encoder.encode(&[]).is_empty());
        assert!(encoder.encode(&[vec![], vec![]]).is_empty());
    }
}
