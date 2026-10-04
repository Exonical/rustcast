//! Forward Error Correction (FEC) for lossy UDP transport.
//!
//! Uses Reed-Solomon erasure coding to generate parity packets from data
//! packets. The receiver can reconstruct any lost data packets as long as
//! at least `data_count` packets (data + parity combined) are received.
//!
//! This is critical for maintaining video quality over unreliable networks
//! without the latency cost of TCP retransmission.
//!
//! Status: placeholder. Reed-Solomon is not implemented yet. The encoder only
//! produces a single XOR parity packet and rejects groups that need more than
//! one; the decoder only handles the no-loss case and returns an error when
//! any packet is missing.

use flux_core::error::{FluxError, Result};

fn not_implemented(what: &str) -> FluxError {
    FluxError::Network(format!("{what}: Reed-Solomon FEC not yet implemented"))
}

/// FEC encoder: generates parity packets from a group of data packets.
pub struct FecEncoder {
    /// Percentage of data packets to generate as parity (0–100).
    fec_percentage: u8,
}

impl FecEncoder {
    pub fn new(fec_percentage: u8) -> Self {
        Self {
            fec_percentage: fec_percentage.min(100),
        }
    }

    /// Calculate the number of parity packets for a given number of data packets.
    pub fn parity_count(&self, data_packet_count: usize) -> usize {
        let count = (data_packet_count * self.fec_percentage as usize + 99) / 100;
        count.max(1) // Always at least 1 parity packet
    }

    /// Generate FEC parity packets for a group of data packets.
    ///
    /// All data packets must be the same length (zero-padded if necessary).
    /// Returns the parity packets. Errors if the group needs more than one
    /// parity packet, since only single XOR parity is implemented.
    pub fn encode(&self, data_packets: &[Vec<u8>]) -> Result<Vec<Vec<u8>>> {
        if data_packets.is_empty() {
            return Ok(vec![]);
        }

        let parity_count = self.parity_count(data_packets.len());
        let packet_size = data_packets.iter().map(|p| p.len()).max().unwrap_or(0);

        if packet_size == 0 {
            return Ok(vec![]);
        }

        if parity_count > 1 {
            return Err(not_implemented(&format!(
                "FEC encode of {} data packets needs {} parity packets",
                data_packets.len(),
                parity_count,
            )));
        }

        // TODO: Replace with reed-solomon-erasure crate:
        //
        //   let encoder = ReedSolomon::new(data_packets.len(), parity_count)?;
        //
        //   // Pad all data packets to the same length
        //   let mut shards: Vec<Vec<u8>> = data_packets
        //       .iter()
        //       .map(|p| {
        //           let mut padded = p.clone();
        //           padded.resize(packet_size, 0);
        //           padded
        //       })
        //       .collect();
        //
        //   // Add empty parity shards
        //   for _ in 0..parity_count {
        //       shards.push(vec![0u8; packet_size]);
        //   }
        //
        //   encoder.encode(&mut shards)?;
        //
        //   // Return only the parity shards
        //   Ok(shards[data_packets.len()..].to_vec())

        tracing::trace!(
            "FEC encode: {} data packets → {} parity packets ({}% overhead)",
            data_packets.len(),
            parity_count,
            self.fec_percentage,
        );

        // Placeholder: XOR-based simple parity (single parity packet)
        let mut parity = vec![0u8; packet_size];
        for packet in data_packets {
            for (i, byte) in packet.iter().enumerate() {
                parity[i] ^= byte;
            }
        }

        Ok(vec![parity])
    }
}

/// FEC decoder: reconstructs lost data packets from received data + parity.
pub struct FecDecoder {
    data_count: usize,
    parity_count: usize,
}

impl FecDecoder {
    pub fn new(data_count: usize, parity_count: usize) -> Self {
        Self {
            data_count,
            parity_count,
        }
    }

    /// Attempt to reconstruct missing data packets.
    ///
    /// `received` contains `(index, data)` pairs for all received packets
    /// (both data and parity). Returns the complete set of data packets if
    /// reconstruction is possible, or an error if too many packets are lost.
    pub fn decode(&self, received: &[(usize, Vec<u8>)]) -> Result<Vec<Vec<u8>>> {
        let total_shards = self.data_count + self.parity_count;
        let missing_count = total_shards - received.len();

        if missing_count > self.parity_count {
            return Err(FluxError::Network(format!(
                "too many lost packets: {} missing but only {} parity available",
                missing_count, self.parity_count,
            )));
        }

        if missing_count == 0 {
            // No loss — just return data packets in order.
            let mut data: Vec<Option<Vec<u8>>> = vec![None; self.data_count];
            for (idx, packet) in received {
                if *idx < self.data_count {
                    data[*idx] = Some(packet.clone());
                }
            }
            return data
                .into_iter()
                .enumerate()
                .map(|(i, p)| p.ok_or_else(|| FluxError::Network(format!("missing data packet {}", i))))
                .collect();
        }

        // TODO: Full Reed-Solomon reconstruction:
        //
        //   let decoder = ReedSolomon::new(self.data_count, self.parity_count)?;
        //
        //   let mut shards: Vec<Option<Vec<u8>>> = vec![None; total_shards];
        //   for (idx, data) in received {
        //       shards[*idx] = Some(data.clone());
        //   }
        //
        //   decoder.reconstruct(&mut shards)?;
        //
        //   // Return reconstructed data shards
        //   shards[..self.data_count]
        //       .iter()
        //       .map(|s| s.clone().unwrap())
        //       .collect()

        Err(not_implemented(&format!(
            "FEC decode of {} missing packets from {} received",
            missing_count,
            received.len(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parity_count_calculation() {
        let encoder = FecEncoder::new(20);
        assert_eq!(encoder.parity_count(10), 2); // 20% of 10 = 2
        assert_eq!(encoder.parity_count(1), 1); // Always at least 1
        assert_eq!(encoder.parity_count(5), 1); // 20% of 5 = 1
    }

    #[test]
    fn xor_parity_basic() {
        let encoder = FecEncoder::new(50);
        let data = vec![vec![0xAA, 0xBB], vec![0xCC, 0xDD]];
        let parity = encoder.encode(&data).unwrap();
        assert!(!parity.is_empty());
        // XOR parity: 0xAA ^ 0xCC = 0x66, 0xBB ^ 0xDD = 0x66
        assert_eq!(parity, vec![vec![0x66, 0x66]]);
    }

    #[test]
    fn encode_rejects_multiple_parity_packets() {
        let encoder = FecEncoder::new(20);
        let data = vec![vec![0x01; 4]; 10];
        assert_eq!(encoder.parity_count(data.len()), 2);
        assert!(matches!(encoder.encode(&data), Err(FluxError::Network(_))));
    }

    #[test]
    fn decode_without_loss_returns_data_in_order() {
        let decoder = FecDecoder::new(2, 1);
        let received = vec![(2, vec![0x66]), (1, vec![0xBB]), (0, vec![0xAA])];
        assert_eq!(decoder.decode(&received).unwrap(), vec![vec![0xAA], vec![0xBB]]);
    }

    #[test]
    fn decode_with_loss_is_not_implemented() {
        let decoder = FecDecoder::new(2, 1);
        let received = vec![(0, vec![0xAA]), (2, vec![0x66])];
        match decoder.decode(&received) {
            Err(FluxError::Network(msg)) => assert!(msg.contains("not yet implemented"), "{msg}"),
            other => panic!("expected not-implemented error, got {other:?}"),
        }
    }
}
