//! H.264 codec parameter construction for the Vulkan Video encoder.
//!
//! Vulkan Video is "stateless": the application supplies the codec std
//! parameter structures (SPS/PPS, per-picture info, slice headers, reference
//! lists) and the driver serializes them into the bitstream. This module
//! builds those `StdVideoH264*` structures (from ash's `vk::native` bindgen
//! bindings of the Vulkan video std headers) for the low-latency IPPP
//! configuration flux uses: single slice, no B-frames, one active reference,
//! POC type 2 (order derived from `frame_num`, no extra slice syntax).

use ash::vk::native::{
    StdVideoEncodeH264PictureInfo, StdVideoEncodeH264ReferenceInfo, StdVideoEncodeH264ReferenceListsInfo,
    StdVideoEncodeH264SliceHeader, StdVideoH264ChromaFormatIdc_STD_VIDEO_H264_CHROMA_FORMAT_IDC_420,
    StdVideoH264LevelIdc, StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_1,
    StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_2, StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_0,
    StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_2, StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_1,
    StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_2, StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_0,
    StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_1, StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_2,
    StdVideoH264PictureParameterSet, StdVideoH264PictureType, StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR,
    StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P, StdVideoH264PocType_STD_VIDEO_H264_POC_TYPE_2,
    StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH, StdVideoH264SequenceParameterSet, StdVideoH264SliceType,
    StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_I, StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_P,
};

/// `log2_max_frame_num_minus4`: the H.264 maximum (12 → 65536 frame numbers)
/// so the modulo counter wraps as rarely as possible with an infinite GOP.
pub const LOG2_MAX_FRAME_NUM_MINUS4: u8 = 12;

/// Sentinel for "no reference picture" in `RefPicList0/1`.
pub const STD_VIDEO_H264_NO_REFERENCE_PICTURE: u8 = 0xff;

/// H.264 macroblock size in luma samples.
const MB_SIZE: u32 = 16;

/// Round `v` up to a whole number of macroblocks.
pub fn mb_count(v: u32) -> u32 {
    v.div_ceil(MB_SIZE)
}

/// Pick the smallest H.264 level that fits the frame size and macroblock
/// rate (Rec. ITU-T H.264 Table A-1; only streaming-relevant levels).
pub fn pick_level(width: u32, height: u32, framerate: u32) -> StdVideoH264LevelIdc {
    let mbs_per_frame = (mb_count(width) * mb_count(height)) as u64;
    let mb_rate = mbs_per_frame * u64::from(framerate.max(1));
    // (level, MaxFS in MBs, MaxMBPS in MB/s)
    const LEVELS: &[(StdVideoH264LevelIdc, u64, u64)] = &[
        (StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_1, 3_600, 108_000),
        (StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_2, 5_120, 216_000),
        (StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_0, 8_192, 245_760),
        (StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_2, 8_704, 522_240),
        (StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_1, 36_864, 983_040),
        (StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_2, 36_864, 2_073_600),
        (StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_0, 139_264, 4_177_920),
        (StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_1, 139_264, 8_355_840),
    ];
    for &(level, max_fs, max_mbps) in LEVELS {
        if mbs_per_frame <= max_fs && mb_rate <= max_mbps {
            return level;
        }
    }
    StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_2
}

/// Build the sequence parameter set for a progressive 4:2:0 8-bit stream.
///
/// POC type 2 derives picture order from `frame_num`, which is valid because
/// every frame in the IPPP structure is a reference frame and there is no
/// reordering.
pub fn build_sps(width: u32, height: u32, framerate: u32) -> StdVideoH264SequenceParameterSet {
    let mut sps: StdVideoH264SequenceParameterSet = unsafe { std::mem::zeroed() };
    sps.profile_idc = StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH;
    sps.level_idc = pick_level(width, height, framerate);
    sps.chroma_format_idc = StdVideoH264ChromaFormatIdc_STD_VIDEO_H264_CHROMA_FORMAT_IDC_420;
    sps.seq_parameter_set_id = 0;
    sps.bit_depth_luma_minus8 = 0;
    sps.bit_depth_chroma_minus8 = 0;
    sps.log2_max_frame_num_minus4 = LOG2_MAX_FRAME_NUM_MINUS4;
    sps.pic_order_cnt_type = StdVideoH264PocType_STD_VIDEO_H264_POC_TYPE_2;
    sps.max_num_ref_frames = 1;
    sps.pic_width_in_mbs_minus1 = mb_count(width) - 1;
    sps.pic_height_in_map_units_minus1 = mb_count(height) - 1;

    // Crop away the macroblock-alignment padding (4:2:0 → units of 2 samples).
    let coded_w = mb_count(width) * MB_SIZE;
    let coded_h = mb_count(height) * MB_SIZE;
    sps.frame_crop_right_offset = (coded_w - width) / 2;
    sps.frame_crop_bottom_offset = (coded_h - height) / 2;

    sps.flags.set_frame_mbs_only_flag(1);
    sps.flags.set_direct_8x8_inference_flag(1);
    if sps.frame_crop_right_offset != 0 || sps.frame_crop_bottom_offset != 0 {
        sps.flags.set_frame_cropping_flag(1);
    }
    sps
}

/// Build the picture parameter set.
///
/// `cabac` selects the entropy coding mode; callers should pass what the
/// driver's `VideoEncodeH264CapabilitiesKHR::std_syntax_flags` allows.
pub fn build_pps(cabac: bool) -> StdVideoH264PictureParameterSet {
    let mut pps: StdVideoH264PictureParameterSet = unsafe { std::mem::zeroed() };
    pps.seq_parameter_set_id = 0;
    pps.pic_parameter_set_id = 0;
    pps.num_ref_idx_l0_default_active_minus1 = 0;
    pps.num_ref_idx_l1_default_active_minus1 = 0;
    pps.pic_init_qp_minus26 = 0;
    if cabac {
        pps.flags.set_entropy_coding_mode_flag(1);
    }
    pps.flags.set_deblocking_filter_control_present_flag(1);
    pps
}

/// Per-frame codec state fed into the std picture/reference structures.
#[derive(Debug, Clone, Copy)]
pub struct FrameState {
    /// Whether this frame is an IDR (key) frame.
    pub is_idr: bool,
    /// `frame_num` syntax element (modulo `2^(log2_max_frame_num_minus4+4)`).
    pub frame_num: u32,
    /// Derived picture order count (POC type 2: `2 * frame_num` for
    /// all-reference streams).
    pub pic_order_cnt: i32,
    /// `idr_pic_id`, incremented per IDR.
    pub idr_pic_id: u16,
}

/// Build the std picture info for the frame being encoded.
///
/// `ref_lists` must outlive the returned struct (it is linked by pointer);
/// pass `None` for IDR frames.
pub fn build_picture_info(
    state: &FrameState,
    ref_lists: Option<&StdVideoEncodeH264ReferenceListsInfo>,
) -> StdVideoEncodeH264PictureInfo {
    let mut info: StdVideoEncodeH264PictureInfo = unsafe { std::mem::zeroed() };
    info.seq_parameter_set_id = 0;
    info.pic_parameter_set_id = 0;
    info.idr_pic_id = state.idr_pic_id;
    info.primary_pic_type = if state.is_idr {
        StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR
    } else {
        StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P
    };
    info.frame_num = state.frame_num;
    info.PicOrderCnt = state.pic_order_cnt;
    // Every frame is a reference in the IPPP prediction structure.
    info.flags.set_is_reference(1);
    if state.is_idr {
        info.flags.set_IdrPicFlag(1);
        // Rebase the DPB: an IDR invalidates all prior references.
        info.flags.set_no_output_of_prior_pics_flag(0);
        info.flags.set_long_term_reference_flag(0);
    }
    if let Some(lists) = ref_lists {
        info.pRefLists = lists;
    }
    info
}

/// Build the L0 reference list pointing at a single DPB slot.
pub fn build_reference_lists(ref_slot: u8) -> StdVideoEncodeH264ReferenceListsInfo {
    let mut lists: StdVideoEncodeH264ReferenceListsInfo = unsafe { std::mem::zeroed() };
    lists.num_ref_idx_l0_active_minus1 = 0;
    lists.num_ref_idx_l1_active_minus1 = 0;
    lists.RefPicList0 = [STD_VIDEO_H264_NO_REFERENCE_PICTURE; 32];
    lists.RefPicList1 = [STD_VIDEO_H264_NO_REFERENCE_PICTURE; 32];
    lists.RefPicList0[0] = ref_slot;
    lists
}

/// Build the std reference info describing a picture in the DPB (used both
/// for the setup slot being written and slots being read).
pub fn build_reference_info(state: &FrameState) -> StdVideoEncodeH264ReferenceInfo {
    let mut info: StdVideoEncodeH264ReferenceInfo = unsafe { std::mem::zeroed() };
    info.primary_pic_type = picture_type(state.is_idr);
    info.FrameNum = state.frame_num;
    info.PicOrderCnt = state.pic_order_cnt;
    info
}

/// Build the single slice header covering the whole frame.
///
/// `qp_delta` is relative to the PPS `pic_init_qp_minus26 + 26` and is only
/// meaningful when rate control is disabled (constant-QP).
pub fn build_slice_header(is_idr: bool, qp_delta: i8) -> StdVideoEncodeH264SliceHeader {
    let mut slice: StdVideoEncodeH264SliceHeader = unsafe { std::mem::zeroed() };
    slice.first_mb_in_slice = 0;
    slice.slice_type = slice_type(is_idr);
    slice.slice_qp_delta = qp_delta;
    slice.cabac_init_idc = 0;
    slice.disable_deblocking_filter_idc = 0;
    slice
}

pub fn slice_type(is_idr: bool) -> StdVideoH264SliceType {
    if is_idr {
        StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_I
    } else {
        StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_P
    }
}

pub fn picture_type(is_idr: bool) -> StdVideoH264PictureType {
    if is_idr {
        StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR
    } else {
        StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_P
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mb_count_rounds_up() {
        assert_eq!(mb_count(1920), 120);
        assert_eq!(mb_count(1080), 68); // 1080 is not MB-aligned
        assert_eq!(mb_count(1), 1);
    }

    #[test]
    fn sps_crops_non_aligned_height() {
        let sps = build_sps(1920, 1080, 60);
        assert_eq!(sps.pic_width_in_mbs_minus1, 119);
        assert_eq!(sps.pic_height_in_map_units_minus1, 67);
        assert_eq!(sps.frame_crop_right_offset, 0);
        // 68 MBs = 1088 rows; 8 padding rows = 4 crop units (4:2:0).
        assert_eq!(sps.frame_crop_bottom_offset, 4);
        assert_eq!(sps.flags.frame_cropping_flag(), 1);
        assert_eq!(sps.flags.frame_mbs_only_flag(), 1);
    }

    #[test]
    fn sps_no_crop_when_aligned() {
        let sps = build_sps(1280, 720, 30);
        assert_eq!(sps.frame_crop_right_offset, 0);
        assert_eq!(sps.frame_crop_bottom_offset, 0);
        assert_eq!(sps.flags.frame_cropping_flag(), 0);
    }

    #[test]
    fn level_scales_with_resolution_and_framerate() {
        assert_eq!(
            pick_level(1280, 720, 30),
            StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_3_1
        );
        assert_eq!(
            pick_level(1920, 1080, 60),
            StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_4_2
        );
        assert_eq!(
            pick_level(3840, 2160, 60),
            StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_5_2
        );
        assert_eq!(
            pick_level(7680, 4320, 60),
            StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_1
        );
    }

    #[test]
    fn idr_picture_info_sets_idr_flag_and_no_refs() {
        let state = FrameState {
            is_idr: true,
            frame_num: 0,
            pic_order_cnt: 0,
            idr_pic_id: 3,
        };
        let info = build_picture_info(&state, None);
        assert_eq!(info.flags.IdrPicFlag(), 1);
        assert_eq!(info.flags.is_reference(), 1);
        assert_eq!(info.idr_pic_id, 3);
        assert!(info.pRefLists.is_null());
        assert_eq!(
            info.primary_pic_type,
            StdVideoH264PictureType_STD_VIDEO_H264_PICTURE_TYPE_IDR
        );
    }

    #[test]
    fn p_picture_links_reference_list() {
        let lists = build_reference_lists(1);
        assert_eq!(lists.RefPicList0[0], 1);
        assert_eq!(lists.RefPicList0[1], STD_VIDEO_H264_NO_REFERENCE_PICTURE);
        assert!(lists
            .RefPicList1
            .iter()
            .all(|&r| r == STD_VIDEO_H264_NO_REFERENCE_PICTURE));

        let state = FrameState {
            is_idr: false,
            frame_num: 5,
            pic_order_cnt: 10,
            idr_pic_id: 0,
        };
        let info = build_picture_info(&state, Some(&lists));
        assert_eq!(info.flags.IdrPicFlag(), 0);
        assert_eq!(info.frame_num, 5);
        assert_eq!(info.PicOrderCnt, 10);
        assert_eq!(info.pRefLists, &lists as *const _);
    }

    #[test]
    fn slice_types_match_picture_kind() {
        assert_eq!(
            build_slice_header(true, 0).slice_type,
            StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_I
        );
        assert_eq!(build_slice_header(false, 2).slice_qp_delta, 2);
        assert_eq!(
            build_slice_header(false, 0).slice_type,
            StdVideoH264SliceType_STD_VIDEO_H264_SLICE_TYPE_P
        );
    }
}
