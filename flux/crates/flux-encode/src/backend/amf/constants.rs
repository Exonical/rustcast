//! AMF encoder property name constants and codec component IDs.
//!
//! These match the defines from the AMF SDK headers:
//!   - components/VideoEncoderVCE.h   (H.264)
//!   - components/VideoEncoderHEVC.h  (H.265)
//!   - components/VideoEncoderAV1.h   (AV1)

#![allow(dead_code)]

// ─── Component IDs (wide-string literals) ───────────────────────────────────

/// H.264 / AVC encoder component ID.
pub const AMF_VIDEO_ENCODER_VCE_AVC: &str = "AMFVideoEncoderVCE_AVC";

/// H.265 / HEVC encoder component ID.
pub const AMF_VIDEO_ENCODER_HEVC: &str = "AMFVideoEncoder_HEVC";

/// AV1 encoder component ID (VCN 4.0+ / RDNA 3+).
pub const AMF_VIDEO_ENCODER_AV1: &str = "AMFVideoEncoder_AV1";

// ─── H.264 encoder properties ───────────────────────────────────────────────

/// Usage mode (amf_int64). Set BEFORE Init().
pub const H264_USAGE: &str = "Usage";
pub const H264_PROFILE: &str = "Profile";
pub const H264_PROFILE_LEVEL: &str = "ProfileLevel";
pub const H264_QUALITY_PRESET: &str = "QualityPreset";

/// Frame size (AMFSize). Set BEFORE Init().
pub const H264_FRAMESIZE: &str = "FrameSize";
/// Frame rate (AMFRate). Set BEFORE Init().
pub const H264_FRAMERATE: &str = "FrameRate";

/// Rate control method (amf_int64).
pub const H264_RATE_CONTROL_METHOD: &str = "RateControlMethod";
/// Target bitrate in bits/sec (amf_int64).
pub const H264_TARGET_BITRATE: &str = "TargetBitrate";
/// Peak bitrate in bits/sec (amf_int64).
pub const H264_PEAK_BITRATE: &str = "PeakBitrate";
/// VBV buffer size in bits (amf_int64).
pub const H264_VBV_BUFFER_SIZE: &str = "VBVBufferSize";
/// Enable filler data (amf_bool).
pub const H264_FILLER_DATA: &str = "FillerDataEnable";
/// Force IDR on next frame (amf_bool).
pub const H264_FORCED_IDR: &str = "ForcedIDR";
/// IDR period in frames (amf_int64). 0 = manual only.
pub const H264_IDR_PERIOD: &str = "IDRPeriod";
/// B-picture pattern (amf_int64). 0 = no B-frames.
pub const H264_B_PIC_PATTERN: &str = "BPicturesPattern";
/// Max number of consecutive B-pictures.
pub const H264_MAX_NUM_REFRAMES: &str = "MaxNumRefFrames";
/// Header insertion mode: "none", "gop", "idr".
pub const H264_HEADER_INSERTION_MODE: &str = "HeaderInsertionMode";
/// Low-latency mode (amf_bool).
pub const H264_LOWLATENCY_MODE: &str = "LowLatencyInternal";
/// Pre-analysis (amf_bool).
pub const H264_PREENCODE: &str = "RateControlPreanalysisEnable";
/// VBAQ (amf_bool).
pub const H264_VBAQ: &str = "EnableVBAQ";
/// Enforce HRD (amf_bool).
pub const H264_ENFORCE_HRD: &str = "EnforceHRD";

/// Per-frame: force picture type (amf_int64).
pub const H264_FORCE_PICTURE_TYPE: &str = "ForcePictureType";
/// Per-frame: insert SPS (amf_bool). Set on surface alongside FORCE_PICTURE_TYPE for IDR.
pub const H264_INSERT_SPS: &str = "InsertSPS";
/// Per-frame: insert PPS (amf_bool). Set on surface alongside FORCE_PICTURE_TYPE for IDR.
pub const H264_INSERT_PPS: &str = "InsertPPS";

// ─── H.265/HEVC encoder properties ─────────────────────────────────────────

pub const HEVC_USAGE: &str = "HevcUsage";
pub const HEVC_PROFILE: &str = "HevcProfile";
pub const HEVC_PROFILE_LEVEL: &str = "HevcProfileLevel";
pub const HEVC_TIER: &str = "HevcTier";
pub const HEVC_QUALITY_PRESET: &str = "HevcQualityPreset";

pub const HEVC_FRAMESIZE: &str = "HevcFrameSize";
pub const HEVC_FRAMERATE: &str = "HevcFrameRate";

pub const HEVC_RATE_CONTROL_METHOD: &str = "HevcRateControlMethod";
pub const HEVC_TARGET_BITRATE: &str = "HevcTargetBitrate";
pub const HEVC_PEAK_BITRATE: &str = "HevcPeakBitrate";
pub const HEVC_VBV_BUFFER_SIZE: &str = "HevcVBVBufferSize";
pub const HEVC_FILLER_DATA: &str = "HevcFillerDataEnable";
pub const HEVC_FORCED_IDR: &str = "HevcForcedIDR";
pub const HEVC_IDR_PERIOD: &str = "HevcIDRPeriod"; // Deprecated — use GOPS_PER_IDR
pub const HEVC_GOPS_PER_IDR: &str = "HevcGOPSPerIDR";
pub const HEVC_GOP_SIZE: &str = "HevcGOPSize";
pub const HEVC_MAX_NUM_REFRAMES: &str = "HevcMaxNumRefFrames";
pub const HEVC_HEADER_INSERTION_MODE: &str = "HevcHeaderInsertionMode";
pub const HEVC_NUM_GOPS_PER_IDR: &str = "HevcNumGOPsPerIDR";
pub const HEVC_PREENCODE: &str = "HevcRateControlPreanalysisEnable";
pub const HEVC_VBAQ: &str = "HevcEnableVBAQ";
pub const HEVC_ENFORCE_HRD: &str = "HevcEnforceHRD";

pub const HEVC_FORCE_PICTURE_TYPE: &str = "HevcForcePictureType";

// ─── AV1 encoder properties ────────────────────────────────────────────────

pub const AV1_USAGE: &str = "Av1Usage";
pub const AV1_PROFILE: &str = "Av1Profile";
pub const AV1_LEVEL: &str = "Av1Level";
pub const AV1_QUALITY_PRESET: &str = "Av1QualityPreset";

pub const AV1_FRAMESIZE: &str = "Av1FrameSize";
pub const AV1_FRAMERATE: &str = "Av1FrameRate";

pub const AV1_RATE_CONTROL_METHOD: &str = "Av1RateControlMethod";
pub const AV1_TARGET_BITRATE: &str = "Av1TargetBitrate";
pub const AV1_PEAK_BITRATE: &str = "Av1PeakBitrate";
pub const AV1_VBV_BUFFER_SIZE: &str = "Av1VBVBufferSize";
pub const AV1_FILLER_DATA: &str = "Av1FillerDataEnable";
pub const AV1_FORCED_IDR: &str = "Av1ForcedIDR";
pub const AV1_PREENCODE: &str = "Av1RateControlPreanalysisEnable";
pub const AV1_ENFORCE_HRD: &str = "Av1EnforceHRD";

pub const AV1_FORCE_PICTURE_TYPE: &str = "Av1ForcePictureType";

// ─── Usage mode values ──────────────────────────────────────────────────────

// H.264 usage modes
pub const H264_USAGE_TRANSCODING: i64 = 0;
pub const H264_USAGE_ULTRA_LOW_LATENCY: i64 = 1;
pub const H264_USAGE_LOW_LATENCY: i64 = 2;
pub const H264_USAGE_WEBCAM: i64 = 3;
pub const H264_USAGE_LOW_LATENCY_HIGH_QUALITY: i64 = 5;

// HEVC usage modes
pub const HEVC_USAGE_TRANSCODING: i64 = 0;
pub const HEVC_USAGE_ULTRA_LOW_LATENCY: i64 = 1;
pub const HEVC_USAGE_LOW_LATENCY: i64 = 2;
pub const HEVC_USAGE_WEBCAM: i64 = 3;
pub const HEVC_USAGE_LOW_LATENCY_HIGH_QUALITY: i64 = 5;

// AV1 usage modes
pub const AV1_USAGE_TRANSCODING: i64 = 0;
pub const AV1_USAGE_LOW_LATENCY: i64 = 1;
pub const AV1_USAGE_ULTRA_LOW_LATENCY: i64 = 2;
pub const AV1_USAGE_WEBCAM: i64 = 3;
pub const AV1_USAGE_LOW_LATENCY_HIGH_QUALITY: i64 = 5;

// ─── Rate control methods ───────────────────────────────────────────────────

// H.264
pub const H264_RC_CQP: i64 = 0;
pub const H264_RC_CBR: i64 = 1;
pub const H264_RC_VBR_PEAK: i64 = 2;
pub const H264_RC_VBR_LATENCY: i64 = 3;

// HEVC
pub const HEVC_RC_CQP: i64 = 0;
pub const HEVC_RC_VBR_LATENCY: i64 = 1;
pub const HEVC_RC_VBR_PEAK: i64 = 2;
pub const HEVC_RC_CBR: i64 = 3;

// AV1
pub const AV1_RC_CQP: i64 = 0;
pub const AV1_RC_VBR_LATENCY: i64 = 1;
pub const AV1_RC_VBR_PEAK: i64 = 2;
pub const AV1_RC_CBR: i64 = 3;

// ─── Quality presets ────────────────────────────────────────────────────────

pub const H264_QUALITY_BALANCED: i64 = 0;
pub const H264_QUALITY_SPEED: i64 = 1;
pub const H264_QUALITY_QUALITY: i64 = 2;

pub const HEVC_QUALITY_QUALITY: i64 = 0;
pub const HEVC_QUALITY_BALANCED: i64 = 5;
pub const HEVC_QUALITY_SPEED: i64 = 10;

pub const AV1_QUALITY_QUALITY: i64 = 30;
pub const AV1_QUALITY_BALANCED: i64 = 70;
pub const AV1_QUALITY_SPEED: i64 = 100;

// ─── Profile values ─────────────────────────────────────────────────────────

pub const H264_PROFILE_BASELINE: i64 = 66;
pub const H264_PROFILE_MAIN: i64 = 77;
pub const H264_PROFILE_HIGH: i64 = 100;
pub const H264_PROFILE_CONSTRAINED_BASELINE: i64 = 256;
pub const H264_PROFILE_CONSTRAINED_HIGH: i64 = 257;

pub const HEVC_PROFILE_MAIN: i64 = 1;
pub const HEVC_PROFILE_MAIN_10: i64 = 2;

pub const HEVC_TIER_MAIN: i64 = 0;
pub const HEVC_TIER_HIGH: i64 = 1;

pub const AV1_PROFILE_MAIN: i64 = 1;

// ─── Picture types ──────────────────────────────────────────────────────────

pub const PICTURE_TYPE_NONE: i64 = 0;
pub const PICTURE_TYPE_SKIP: i64 = 1;
pub const PICTURE_TYPE_IDR: i64 = 2;
pub const PICTURE_TYPE_I: i64 = 3;
pub const PICTURE_TYPE_P: i64 = 4;
pub const PICTURE_TYPE_B: i64 = 5;

// ─── Output data type (read from output buffer) ────────────────────────────

pub const H264_OUTPUT_DATA_TYPE: &str = "OutputDataType";
pub const HEVC_OUTPUT_DATA_TYPE: &str = "OutputDataType";

pub const OUTPUT_DATA_TYPE_IDR: i64 = 0;
pub const OUTPUT_DATA_TYPE_I: i64 = 1;
pub const OUTPUT_DATA_TYPE_P: i64 = 2;
pub const OUTPUT_DATA_TYPE_B: i64 = 3;

// ─── Header insertion mode ──────────────────────────────────────────────────

pub const HEADER_INSERTION_NONE: i64 = 0;
pub const HEADER_INSERTION_GOP: i64 = 1;
pub const HEADER_INSERTION_IDR: i64 = 2;
