//! Video decoder for the client side.
//!
//! Decodes compressed video frames received from the server. Uses GPU-accelerated
//! decoding when available (DXVA2/D3D11VA on Windows, VA-API/VDPAU on Linux,
//! Vulkan Video decode).

use flux_core::error::Result;
use flux_core::types::{Resolution, VideoCodec};

/// Decoded video frame ready for rendering.
pub struct DecodedFrame {
    /// Frame index.
    pub index: u64,

    /// Resolution.
    pub resolution: Resolution,

    /// Decoded pixel data (NV12 or platform-specific GPU texture handle).
    pub data: Vec<u8>,

    /// Whether this frame is GPU-resident (texture handle in `data` field).
    pub gpu_resident: bool,
}

/// Video decoder configuration.
#[derive(Debug, Clone)]
pub struct DecoderConfig {
    pub codec: VideoCodec,
    pub resolution: Resolution,
}

/// Video decoder that converts compressed bitstream to renderable frames.
pub struct VideoDecoder {
    config: DecoderConfig,
    frame_count: u64,
    // TODO: Decoder backend handle:
    //   Windows: ID3D11VideoDecoder (D3D11VA) or Vulkan Video decode session
    //   Linux:   VAContext (VA-API) or Vulkan Video decode session
    //   Fallback: FFmpeg libavcodec software decoder
}

impl VideoDecoder {
    pub fn new(config: DecoderConfig) -> Result<Self> {
        tracing::info!(
            "Initializing video decoder: {} {}",
            config.codec,
            config.resolution,
        );

        // TODO: Decoder initialization:
        //
        //   1. Try GPU-accelerated decode first:
        //
        //      Windows (D3D11VA):
        //        - Create ID3D11VideoDevice from the D3D11 device
        //        - Create ID3D11VideoDecoder with appropriate profile:
        //          H.264 → D3D11_DECODER_PROFILE_H264_VLD_NOFGT
        //          H.265 → D3D11_DECODER_PROFILE_HEVC_VLD_MAIN
        //        - Allocate output textures (NV12 format)
        //
        //      Linux (VA-API):
        //        - vaCreateConfig with decode entrypoint
        //        - vaCreateSurfaces for output
        //        - vaCreateContext
        //
        //      Vulkan Video Decode:
        //        - VK_KHR_video_decode_queue + VK_KHR_video_decode_h264/h265
        //        - Similar to encode but for decode direction
        //
        //   2. Fallback to FFmpeg:
        //        - avcodec_find_decoder(AV_CODEC_ID_H264/HEVC/AV1)
        //        - avcodec_alloc_context3, avcodec_open2

        Ok(Self {
            config,
            frame_count: 0,
        })
    }

    /// Decode a compressed video frame.
    pub fn decode(&mut self, data: &[u8], is_keyframe: bool) -> Result<Option<DecodedFrame>> {
        self.frame_count += 1;

        tracing::trace!(
            "Decoding frame {} ({} bytes, keyframe={})",
            self.frame_count,
            data.len(),
            is_keyframe,
        );

        // TODO: Feed compressed data to decoder:
        //
        //   GPU decode:
        //     1. Parse NAL units from the bitstream
        //     2. Submit to hardware decoder
        //     3. Wait for decoded output
        //     4. Return GPU texture handle or mapped pixel data
        //
        //   Software decode:
        //     1. av_packet_from_data
        //     2. avcodec_send_packet
        //     3. avcodec_receive_frame
        //     4. Return decoded YUV data

        Ok(Some(DecodedFrame {
            index: self.frame_count,
            resolution: self.config.resolution,
            data: Vec::new(), // placeholder
            gpu_resident: false,
        }))
    }

    /// Flush any remaining frames from the decoder.
    pub fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        tracing::debug!("Flushing video decoder");
        Ok(vec![])
    }
}
