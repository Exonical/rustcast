//! Linux VA-API encoder backend.
//!
//! Uses the Video Acceleration API for hardware-accelerated encoding on Linux.
//! Works with both AMD (via Mesa radeonsi/AMDGPU) and Intel (via iHD/i965)
//! GPUs. Supports H.264 and H.265/HEVC (AV1 on newer Intel GPUs).

use flux_core::error::{FluxError, Result};
use flux_core::frame::{CapturedFrame, EncodedPacket};
use flux_core::types::{Resolution, VideoCodec};

use crate::traits::{EncodeConfig, EncodeSession, EncoderCapabilities, VideoEncoder};

/// VA-API hardware encoder (Linux).
pub struct VaapiEncoder {
    // TODO: Store VA display and config:
    //   - VADisplay (from DRM fd via vaGetDisplayDRM)
    //   - VAConfigID
    //   - DRM render node fd
    _private: (),
}

impl VaapiEncoder {
    pub fn new() -> Result<Self> {
        tracing::info!("Initializing VA-API encoder");

        // TODO: Initialization:
        //   1. Open DRM render node: /dev/dri/renderD128
        //   2. vaGetDisplayDRM(fd) → VADisplay
        //   3. vaInitialize(display) — check version
        //   4. vaQueryConfigEntrypoints — enumerate supported codec profiles
        //   5. vaQueryConfigAttributes — check encoding capabilities

        Ok(Self { _private: () })
    }
}

impl VideoEncoder for VaapiEncoder {
    fn name(&self) -> &'static str {
        "VA-API"
    }

    fn capabilities(&self) -> Result<EncoderCapabilities> {
        // TODO: Query via vaQueryConfigProfiles + vaQueryConfigEntrypoints
        Ok(EncoderCapabilities {
            name: "VA-API",
            supported_codecs: vec![VideoCodec::H264, VideoCodec::H265],
            supports_hdr: true,
            supports_yuv444: false,
            max_resolution: Resolution::new(7680, 4320),
            max_framerate: 240,
        })
    }

    fn validate_config(&self, config: &EncodeConfig) -> Result<()> {
        if config.codec == VideoCodec::Av1 {
            return Err(FluxError::EncoderInit(
                "VA-API AV1 encoding requires Intel Arc or newer".into(),
            ));
        }
        Ok(())
    }

    fn create_session(&self, config: EncodeConfig) -> Result<Box<dyn EncodeSession>> {
        Ok(Box::new(VaapiSession::new(config)?))
    }
}

struct VaapiSession {
    config: EncodeConfig,
    frame_index: u64,
    idr_requested: bool,
    // TODO: VAContextID, VASurfaceID pool, VABufferID for params/bitstream
}

impl VaapiSession {
    fn new(config: EncodeConfig) -> Result<Self> {
        tracing::info!(
            "Creating VA-API session: {} {}@{}fps {}kbps",
            config.codec,
            config.resolution,
            config.framerate,
            config.bitrate_kbps,
        );

        // TODO: Full VA-API session setup:
        //
        //   1. Select VA profile:
        //      - H.264 → VAProfileH264High
        //      - H.265 → VAProfileHEVCMain (or Main10 for HDR)
        //
        //   2. vaCreateConfig(display, profile, VAEntrypointEncSlice, attribs)
        //
        //   3. Create surfaces for input (NV12/P010):
        //      vaCreateSurfaces(VA_RT_FORMAT_YUV420, width, height, surfaces, count)
        //      For DMA-BUF zero-copy: vaCreateSurfaces with VASurfaceAttribExternalBuffers
        //
        //   4. vaCreateContext(display, config, width, height, VA_PROGRESSIVE, surfaces)
        //
        //   5. Configure rate control:
        //      - VAEncMiscParameterTypeRateControl → CBR/VBR params
        //      - VAEncMiscParameterTypeFrameRate → framerate
        //      - VAEncMiscParameterTypeHRD → VBV buffer size

        Ok(Self {
            config,
            frame_index: 0,
            idr_requested: true,
        })
    }
}

impl EncodeSession for VaapiSession {
    fn encode(&mut self, frame: &CapturedFrame) -> Result<Vec<EncodedPacket>> {
        self.frame_index += 1;
        let is_idr = self.idr_requested;
        self.idr_requested = false;

        // TODO: VA-API encode pipeline:
        //
        //   1. Import / upload frame to VASurface:
        //      a. DMA-BUF zero-copy: vaCreateSurfaces with external fd
        //      b. CPU: vaMapBuffer on the surface → memcpy NV12 planes
        //
        //   2. Begin picture:
        //      vaBeginPicture(context, surface)
        //
        //   3. Create and render parameter buffers:
        //      - VAEncSequenceParameterBuffer (SPS — on IDR frames)
        //      - VAEncPictureParameterBuffer (PPS)
        //      - VAEncSliceParameterBuffer (slice header)
        //      - VAEncMiscParameterBuffer (rate control, etc.)
        //      vaRenderPicture(context, buffers, count)
        //
        //   4. vaEndPicture(context) — triggers encoding
        //
        //   5. vaSyncSurface(surface) — wait for completion
        //
        //   6. Extract bitstream:
        //      vaMapBuffer(coded_buf) → read data → vaUnmapBuffer

        tracing::trace!("VA-API encode frame {} (IDR={})", self.frame_index, is_idr);

        Ok(vec![EncodedPacket {
            frame_index: self.frame_index,
            pts: self.frame_index,
            is_keyframe: is_idr,
            data: Vec::new(),
        }])
    }

    fn request_idr(&mut self) {
        tracing::debug!("VA-API: IDR frame requested");
        self.idr_requested = true;
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        tracing::debug!("VA-API: flushing encoder");
        Ok(vec![])
    }

    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()> {
        // TODO: Update VAEncMiscParameterTypeRateControl dynamically
        tracing::info!("VA-API: bitrate updated to {} kbps", bitrate_kbps);
        self.config.bitrate_kbps = bitrate_kbps;
        Ok(())
    }
}
