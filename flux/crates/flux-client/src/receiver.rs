//! Network receiver for video and audio RTP streams.
//!
//! Manages UDP sockets, depacketizes RTP, performs FEC reconstruction,
//! and feeds decoded frames to the renderer.

use std::net::SocketAddr;

use flux_core::error::{FluxError, Result};
use flux_transport::packetizer::{Depacketizer, ReassembledFrame};

/// Video stream receiver.
pub struct VideoReceiver {
    depacketizer: Depacketizer,
    server_addr: Option<SocketAddr>,
    frames_received: u64,
    frames_lost: u64,
    // TODO: UdpSocket, FEC decoder, jitter buffer
}

impl VideoReceiver {
    pub fn new() -> Self {
        Self {
            depacketizer: Depacketizer::new(),
            server_addr: None,
            frames_received: 0,
            frames_lost: 0,
        }
    }

    /// Bind the UDP socket and start receiving.
    pub async fn bind(&mut self, local_addr: SocketAddr) -> Result<()> {
        tracing::info!("Binding video receiver on {}", local_addr);
        // TODO: tokio::net::UdpSocket::bind(local_addr)
        Ok(())
    }

    /// Set the server address to receive from.
    pub fn set_server_addr(&mut self, addr: SocketAddr) {
        self.server_addr = Some(addr);
    }

    /// Receive and depacketize the next video frame.
    ///
    /// This blocks until a complete frame is assembled from RTP packets.
    pub async fn recv_frame(&mut self) -> Result<ReassembledFrame> {
        // TODO: Full receive loop:
        //
        //   loop {
        //       let (data, addr) = socket.recv_from(&mut buf).await?;
        //
        //       // Validate source address
        //       if Some(addr) != self.server_addr {
        //           continue;
        //       }
        //
        //       // Feed to depacketizer
        //       if let Some(frame) = self.depacketizer.feed(&data) {
        //           self.frames_received += 1;
        //           return Ok(frame);
        //       }
        //
        //       // TODO: Check for frame timeout → attempt FEC reconstruction
        //       // TODO: If FEC fails, increment frames_lost and request IDR
        //   }

        Err(FluxError::Network("video receiver not yet implemented".into()))
    }

    /// Get receiver statistics.
    pub fn stats(&self) -> ReceiverStats {
        ReceiverStats {
            frames_received: self.frames_received,
            frames_lost: self.frames_lost,
        }
    }
}

/// Audio stream receiver.
pub struct AudioReceiver {
    server_addr: Option<SocketAddr>,
    packets_received: u64,
    // TODO: UdpSocket, Opus decoder, jitter buffer, audio output device
}

impl AudioReceiver {
    pub fn new() -> Self {
        Self {
            server_addr: None,
            packets_received: 0,
        }
    }

    /// Bind the UDP socket for audio.
    pub async fn bind(&mut self, local_addr: SocketAddr) -> Result<()> {
        tracing::info!("Binding audio receiver on {}", local_addr);
        Ok(())
    }

    /// Receive, decode, and play audio.
    pub async fn recv_and_play(&mut self) -> Result<()> {
        // TODO: Receive loop:
        //   1. Recv RTP packet from UDP
        //   2. Strip RTP header
        //   3. Opus decode: opus_decode_float
        //   4. Write to audio output device:
        //      Windows: IAudioRenderClient
        //      Linux: PipeWire / PulseAudio playback
        //   5. Handle jitter with a small ring buffer

        Err(FluxError::Network("audio receiver not yet implemented".into()))
    }
}

/// Network receiver statistics.
#[derive(Debug, Clone)]
pub struct ReceiverStats {
    pub frames_received: u64,
    pub frames_lost: u64,
}

impl ReceiverStats {
    pub fn loss_rate(&self) -> f64 {
        let total = self.frames_received + self.frames_lost;
        if total == 0 {
            0.0
        } else {
            self.frames_lost as f64 / total as f64
        }
    }
}
