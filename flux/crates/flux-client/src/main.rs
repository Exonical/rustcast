#![allow(dead_code)]

use clap::Parser;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

mod decoder;
mod receiver;
mod renderer;

use flux_core::types::{Resolution, VideoCodec};
use flux_protocol::messages::*;
use flux_protocol::version;

#[derive(Parser, Debug)]
#[clap(name = "flux-client", version, about = "Flux Remote Streaming Client")]
struct Args {
    /// Server address (host:port).
    #[clap(short, long)]
    server: String,

    /// Client display name.
    #[clap(short, long, default_value = "Flux Client")]
    name: String,

    /// Preferred video codec.
    #[clap(long, default_value = "h265")]
    codec: String,

    /// Requested resolution (WIDTHxHEIGHT).
    #[clap(long, default_value = "1920x1080")]
    resolution: String,

    /// Requested framerate.
    #[clap(long, default_value = "60")]
    fps: u32,

    /// Requested video bitrate in kbps.
    #[clap(long, default_value = "20000")]
    bitrate: u32,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(EnvFilter::from_default_env().add_directive("flux=info".parse()?))
        .init();

    let args = Args::parse();

    let codec = match args.codec.to_lowercase().as_str() {
        "h264" => VideoCodec::H264,
        "h265" | "hevc" => VideoCodec::H265,
        "av1" => VideoCodec::Av1,
        other => {
            tracing::error!("Unknown codec: {}", other);
            return Err(format!("unknown codec: {}", other).into());
        }
    };

    let resolution = parse_resolution(&args.resolution)?;

    tracing::info!(
        "Flux Client '{}' connecting to {} ({} {}@{}fps {}kbps)",
        args.name,
        args.server,
        codec,
        resolution,
        args.fps,
        args.bitrate,
    );

    // TODO: Full client connection flow:
    //
    //   1. Resolve server address
    //   2. Establish QUIC connection (via flux-transport)
    //   3. Send Hello message
    //   4. Receive Welcome message
    //   5. Handle pairing if required (PIN entry)
    //   6. Send SessionRequest
    //   7. Receive SessionAccepted with negotiated params and port info
    //   8. Start media receivers (video + audio UDP sockets)
    //   9. Start input sender
    //  10. Enter main loop:
    //      a. Receive video RTP packets → depacketize → FEC reconstruct → decode → render
    //      b. Receive audio RTP packets → decode → play
    //      c. Capture local input → serialize → send to server
    //      d. Handle keepalive ping/pong
    //      e. Handle bitrate adaptation based on network conditions

    let client = FluxClient::new(args.name, codec, resolution, args.fps, args.bitrate);

    match client.connect(&args.server).await {
        Ok(()) => tracing::info!("Session ended normally"),
        Err(e) => tracing::error!("Session error: {}", e),
    }

    Ok(())
}

struct FluxClient {
    name: String,
    codec: VideoCodec,
    resolution: Resolution,
    fps: u32,
    bitrate_kbps: u32,
}

impl FluxClient {
    fn new(
        name: String,
        codec: VideoCodec,
        resolution: Resolution,
        fps: u32,
        bitrate_kbps: u32,
    ) -> Self {
        Self {
            name,
            codec,
            resolution,
            fps,
            bitrate_kbps,
        }
    }

    async fn connect(&self, server_addr: &str) -> Result<(), Box<dyn std::error::Error>> {
        tracing::info!("Connecting to {}", server_addr);

        // Build Hello message.
        let hello = HelloMessage {
            protocol_version: version::PROTOCOL_VERSION,
            client_name: self.name.clone(),
            supported_codecs: vec![self.codec],
            max_resolution: self.resolution,
            max_fps: self.fps,
            supports_hdr: false,
        };

        tracing::debug!("Sending Hello: {:?}", hello);

        // TODO: Establish QUIC connection and send Hello.
        // TODO: Receive Welcome, negotiate session, start streaming.
        // TODO: Create decoder, renderer, and input handler.
        // TODO: Enter main receive/render loop.

        tracing::info!("Connection flow not yet fully implemented");
        Ok(())
    }
}

fn parse_resolution(s: &str) -> Result<Resolution, Box<dyn std::error::Error>> {
    let parts: Vec<&str> = s.split('x').collect();
    if parts.len() != 2 {
        return Err(format!("invalid resolution format '{}', expected WIDTHxHEIGHT", s).into());
    }
    let width: u32 = parts[0].parse()?;
    let height: u32 = parts[1].parse()?;
    Ok(Resolution::new(width, height))
}
