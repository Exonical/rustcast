use clap::Parser;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

// Decoder and renderer are scaffolding that `FluxClient::connect` does not use yet.
#[allow(dead_code)]
mod decoder;
#[allow(dead_code)]
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

    let client = FluxClient::new(args.name, codec, resolution, args.fps, args.bitrate);

    if let Err(e) = client.connect(&args.server).await {
        tracing::error!("Session error: {}", e);
        return Err(e);
    }

    tracing::info!("Session ended normally");
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

    fn hello(&self) -> HelloMessage {
        HelloMessage {
            protocol_version: version::PROTOCOL_VERSION,
            client_name: self.name.clone(),
            supported_codecs: vec![self.codec],
            max_resolution: self.resolution,
            max_fps: self.fps,
            supports_hdr: false,
        }
    }

    /// Not implemented: there is no transport, session negotiation, or media path yet,
    /// so this always fails instead of pretending a session ran.
    async fn connect(&self, server_addr: &str) -> Result<(), Box<dyn std::error::Error>> {
        tracing::debug!(
            "Would send Hello to {} at {} kbps: {:?}",
            server_addr,
            self.bitrate_kbps,
            self.hello()
        );
        Err(format!("cannot connect to {server_addr}: flux-client connection flow is not implemented").into())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_fails_instead_of_faking_a_session() {
        let client = FluxClient::new("test".into(), VideoCodec::H264, Resolution::new(1280, 720), 30, 5000);
        let err = client.connect("127.0.0.1:8443").await.unwrap_err();
        assert!(err.to_string().contains("not implemented"), "unexpected error: {err}");
    }
}
