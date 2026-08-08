//! Real Vulkan Video encoder built directly on ash's raw
//! `VK_KHR_video_encode_queue` bindings.
//!
//! Requires a Vulkan 1.3 driver exposing `VK_KHR_video_queue`,
//! `VK_KHR_video_encode_queue` and `VK_KHR_video_encode_h264` (Mesa RADV/ANV
//! 24.1+, NVIDIA 550+). H.264 only for now; H.265/AV1 are later phases.
//!
//! Pipeline per frame (low-latency IPPP, 1 reference, single slice):
//!   1. convert the captured frame to NV12 on the CPU and copy into a staging
//!      buffer (DMA-BUF zero-copy import is a later step),
//!   2. transfer queue: staging buffer → NV12 `VIDEO_ENCODE_SRC` image,
//!   3. encode queue: `vkCmdEncodeVideoKHR` into a host-visible bitstream
//!      buffer, with a 2-slot DPB (setup slot + previous reference),
//!   4. read back the encoded size via a `VIDEO_ENCODE_FEEDBACK` query and
//!      copy the bitstream out (prepending SPS/PPS on IDR frames).

use std::ffi::c_void;
use std::sync::Arc;

use ash::vk;
use parking_lot::Mutex;

use flux_core::error::{FluxError, Result};
use flux_core::frame::{CapturedFrame, EncodedPacket};
use flux_core::types::{RateControlMode, Resolution, VideoCodec};

use crate::nv12::frame_to_nv12;
use crate::traits::{EncodeConfig, EncodeSession, EncoderCapabilities, VideoEncoder};

use super::h264;

/// Keyframe interval used when the config requests an infinite GOP, so
/// periodic IDRs still recover from packet loss.
const INFINITE_GOP_IDR_INTERVAL: u64 = 2048;

/// Constant QP used for `RateControlMode::Cqp` (H.264 "medium quality").
const CONSTANT_QP: i32 = 23;

/// Minimum bitstream buffer size (worst-case IDR at high bitrate).
const MIN_BITSTREAM_BUFFER_SIZE: vk::DeviceSize = 4 * 1024 * 1024;

fn init_err(what: &str, e: vk::Result) -> FluxError {
    FluxError::EncoderInit(format!("Vulkan Video: {what} failed: {e:?}"))
}

// ───────────────────────────── profile chain ─────────────────────────────

/// The H.264 encode video profile with its full pNext chain, boxed so the
/// intra-struct pointers stay valid. The same chain must be passed everywhere
/// a profile is required (capability queries, session, images, buffers,
/// query pool) for the driver to treat the resources as compatible.
struct H264ProfileChain {
    usage: vk::VideoEncodeUsageInfoKHR<'static>,
    h264: vk::VideoEncodeH264ProfileInfoKHR<'static>,
    profile: vk::VideoProfileInfoKHR<'static>,
    list: vk::VideoProfileListInfoKHR<'static>,
}

impl H264ProfileChain {
    fn new() -> Box<Self> {
        let mut chain = Box::new(Self {
            usage: vk::VideoEncodeUsageInfoKHR::default()
                .video_usage_hints(vk::VideoEncodeUsageFlagsKHR::STREAMING)
                .video_content_hints(vk::VideoEncodeContentFlagsKHR::DESKTOP)
                .tuning_mode(vk::VideoEncodeTuningModeKHR::LOW_LATENCY),
            h264: vk::VideoEncodeH264ProfileInfoKHR::default()
                .std_profile_idc(vk::native::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH),
            profile: vk::VideoProfileInfoKHR::default()
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
                .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
                .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
                .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8),
            list: vk::VideoProfileListInfoKHR::default(),
        });
        chain.h264.p_next = &chain.usage as *const _ as *const c_void;
        chain.profile.p_next = &chain.h264 as *const _ as *const c_void;
        chain.list.profile_count = 1;
        chain.list.p_profiles = &chain.profile;
        chain
    }

    fn profile(&self) -> &vk::VideoProfileInfoKHR<'static> {
        &self.profile
    }

    /// Pointer to the profile list, for chaining onto image/buffer create infos.
    fn list_ptr(&self) -> *const c_void {
        &self.list as *const _ as *const c_void
    }
}

// ───────────────────────────── context ─────────────────────────────

/// Shared Vulkan instance/device state, owned by the encoder and kept alive
/// by sessions through an `Arc`.
struct VulkanContext {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    video_queue_fn: ash::khr::video_queue::DeviceFn,
    encode_queue_fn: ash::khr::video_encode_queue::DeviceFn,
    /// Vulkan queues need external synchronization; sessions lock to submit.
    encode_queue: Mutex<vk::Queue>,
    transfer_queue: Mutex<vk::Queue>,
    encode_qf: u32,
    transfer_qf: u32,
    memory_props: vk::PhysicalDeviceMemoryProperties,
}

impl Drop for VulkanContext {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

impl VulkanContext {
    fn find_memory_type(&self, type_bits: u32, flags: vk::MemoryPropertyFlags) -> Result<u32> {
        (0..self.memory_props.memory_type_count)
            .find(|&i| {
                type_bits & (1 << i) != 0
                    && self.memory_props.memory_types[i as usize]
                        .property_flags
                        .contains(flags)
            })
            .ok_or_else(|| FluxError::EncoderInit(format!("Vulkan Video: no memory type matching {flags:?}")))
    }
}

/// Capability limits captured at probe time and reused by sessions.
#[derive(Clone, Copy)]
struct H264Caps {
    max_coded_extent: vk::Extent2D,
    min_bitstream_size_align: vk::DeviceSize,
    max_dpb_slots: u32,
    max_active_refs: u32,
    std_header_version: vk::ExtensionProperties,
    rate_control_modes: vk::VideoEncodeRateControlModeFlagsKHR,
    min_qp: i32,
    max_qp: i32,
    cabac: bool,
}

// ───────────────────────────── encoder ─────────────────────────────

/// Vulkan Video hardware encoder (cross-platform, cross-vendor).
pub struct VulkanVideoEncoder {
    ctx: Arc<VulkanContext>,
    caps: H264Caps,
    device_name: String,
}

impl VulkanVideoEncoder {
    pub fn new() -> Result<Self> {
        tracing::info!("Initializing Vulkan Video encoder (ash)");
        let entry = unsafe { ash::Entry::load() }
            .map_err(|e| FluxError::EncoderInit(format!("Vulkan loader unavailable: {e}")))?;

        let app_info = vk::ApplicationInfo::default()
            .application_name(c"rustcast")
            .api_version(vk::API_VERSION_1_3);
        let instance_info = vk::InstanceCreateInfo::default().application_info(&app_info);
        let instance =
            unsafe { entry.create_instance(&instance_info, None) }.map_err(|e| init_err("vkCreateInstance", e))?;

        match Self::init_device(entry, instance) {
            Ok(enc) => Ok(enc),
            Err(e) => Err(e),
        }
    }

    fn init_device(entry: ash::Entry, instance: ash::Instance) -> Result<Self> {
        // Destroy the instance on any init failure so we don't leak it.
        let result = Self::try_init_device(&entry, &instance);
        match result {
            Ok((physical_device, device, encode_qf, transfer_qf, caps, device_name)) => {
                let video_queue_fn = ash::khr::video_queue::Device::new(&instance, &device).fp().clone();
                let encode_queue_fn = ash::khr::video_encode_queue::Device::new(&instance, &device)
                    .fp()
                    .clone();
                let encode_queue = unsafe { device.get_device_queue(encode_qf, 0) };
                let transfer_queue = if transfer_qf == encode_qf {
                    encode_queue
                } else {
                    unsafe { device.get_device_queue(transfer_qf, 0) }
                };
                let memory_props = unsafe { instance.get_physical_device_memory_properties(physical_device) };
                tracing::info!(
                    device = %device_name,
                    encode_qf,
                    transfer_qf,
                    "Vulkan Video encoder ready (H.264)"
                );
                Ok(Self {
                    ctx: Arc::new(VulkanContext {
                        _entry: entry,
                        instance,
                        device,
                        video_queue_fn,
                        encode_queue_fn,
                        encode_queue: Mutex::new(encode_queue),
                        transfer_queue: Mutex::new(transfer_queue),
                        encode_qf,
                        transfer_qf,
                        memory_props,
                    }),
                    caps,
                    device_name,
                })
            }
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                Err(e)
            }
        }
    }

    #[allow(clippy::type_complexity)]
    fn try_init_device(
        entry: &ash::Entry,
        instance: &ash::Instance,
    ) -> Result<(vk::PhysicalDevice, ash::Device, u32, u32, H264Caps, String)> {
        let devices =
            unsafe { instance.enumerate_physical_devices() }.map_err(|e| init_err("vkEnumeratePhysicalDevices", e))?;

        let required_exts = [
            vk::KHR_VIDEO_QUEUE_NAME,
            vk::KHR_VIDEO_ENCODE_QUEUE_NAME,
            vk::KHR_VIDEO_ENCODE_H264_NAME,
        ];

        for pd in devices {
            let props = unsafe { instance.get_physical_device_properties(pd) };
            if props.api_version < vk::API_VERSION_1_3 {
                continue;
            }
            let exts = unsafe { instance.enumerate_device_extension_properties(pd) }.unwrap_or_default();
            let has = |name: &std::ffi::CStr| exts.iter().any(|e| e.extension_name_as_c_str() == Ok(name));
            if !required_exts.iter().all(|n| has(n)) {
                continue;
            }

            let Some((encode_qf, transfer_qf)) = Self::find_queue_families(instance, pd) else {
                continue;
            };

            let caps = match Self::query_h264_caps(entry, instance, pd) {
                Ok(caps) => caps,
                Err(e) => {
                    tracing::debug!("Vulkan Video: skipping device: {e}");
                    continue;
                }
            };

            let device_name = props
                .device_name_as_c_str()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();

            let device = Self::create_device(instance, pd, encode_qf, transfer_qf, &required_exts)?;
            return Ok((pd, device, encode_qf, transfer_qf, caps, device_name));
        }

        Err(FluxError::EncoderInit(
            "no Vulkan device with H.264 video encode support found".into(),
        ))
    }

    /// Find (encode queue family, transfer queue family). The transfer family
    /// handles the staging upload; prefer a non-encode family so the copy and
    /// encode can overlap.
    fn find_queue_families(instance: &ash::Instance, pd: vk::PhysicalDevice) -> Option<(u32, u32)> {
        let count = unsafe { instance.get_physical_device_queue_family_properties(pd) }.len();
        let mut video_props = vec![vk::QueueFamilyVideoPropertiesKHR::default(); count];
        let mut props2: Vec<vk::QueueFamilyProperties2> = video_props
            .iter_mut()
            .map(|v| vk::QueueFamilyProperties2::default().push_next(v))
            .collect();
        unsafe { instance.get_physical_device_queue_family_properties2(pd, &mut props2) };
        let family_flags: Vec<vk::QueueFlags> = props2.iter().map(|p| p.queue_family_properties.queue_flags).collect();
        drop(props2);

        let mut encode_qf = None;
        let mut transfer_qf = None;
        for (i, &flags) in family_flags.iter().enumerate() {
            if encode_qf.is_none()
                && flags.contains(vk::QueueFlags::VIDEO_ENCODE_KHR)
                && video_props[i]
                    .video_codec_operations
                    .contains(vk::VideoCodecOperationFlagsKHR::ENCODE_H264)
            {
                encode_qf = Some(i as u32);
            }
            if transfer_qf.is_none()
                && flags.intersects(vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE | vk::QueueFlags::TRANSFER)
                && !flags.contains(vk::QueueFlags::VIDEO_ENCODE_KHR)
            {
                transfer_qf = Some(i as u32);
            }
        }
        let encode = encode_qf?;
        Some((encode, transfer_qf.unwrap_or(encode)))
    }

    fn query_h264_caps(entry: &ash::Entry, instance: &ash::Instance, pd: vk::PhysicalDevice) -> Result<H264Caps> {
        let video_instance_fn = ash::khr::video_queue::Instance::new(entry, instance);
        let profile_chain = H264ProfileChain::new();

        let mut h264_caps = vk::VideoEncodeH264CapabilitiesKHR::default();
        let mut encode_caps = vk::VideoEncodeCapabilitiesKHR {
            p_next: &mut h264_caps as *mut _ as *mut c_void,
            ..Default::default()
        };
        let mut caps = vk::VideoCapabilitiesKHR {
            p_next: &mut encode_caps as *mut _ as *mut c_void,
            ..Default::default()
        };

        unsafe {
            (video_instance_fn.fp().get_physical_device_video_capabilities_khr)(pd, profile_chain.profile(), &mut caps)
        }
        .result()
        .map_err(|e| init_err("vkGetPhysicalDeviceVideoCapabilitiesKHR", e))?;

        // Verify NV12 is a supported encode input format for this profile.
        let mut format_info =
            vk::PhysicalDeviceVideoFormatInfoKHR::default().image_usage(vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR);
        format_info.p_next = profile_chain.list_ptr();
        let mut format_count = 0u32;
        unsafe {
            (video_instance_fn.fp().get_physical_device_video_format_properties_khr)(
                pd,
                &format_info,
                &mut format_count,
                std::ptr::null_mut(),
            )
        }
        .result()
        .map_err(|e| init_err("vkGetPhysicalDeviceVideoFormatPropertiesKHR", e))?;
        let mut formats = vec![vk::VideoFormatPropertiesKHR::default(); format_count as usize];
        unsafe {
            (video_instance_fn.fp().get_physical_device_video_format_properties_khr)(
                pd,
                &format_info,
                &mut format_count,
                formats.as_mut_ptr(),
            )
        }
        .result()
        .map_err(|e| init_err("vkGetPhysicalDeviceVideoFormatPropertiesKHR", e))?;
        if !formats.iter().any(|f| f.format == vk::Format::G8_B8R8_2PLANE_420_UNORM) {
            return Err(FluxError::EncoderInit(
                "Vulkan Video: driver does not support NV12 encode input".into(),
            ));
        }

        Ok(H264Caps {
            max_coded_extent: caps.max_coded_extent,
            min_bitstream_size_align: caps.min_bitstream_buffer_size_alignment,
            max_dpb_slots: caps.max_dpb_slots,
            max_active_refs: caps.max_active_reference_pictures,
            std_header_version: caps.std_header_version,
            rate_control_modes: encode_caps.rate_control_modes,
            min_qp: h264_caps.min_qp,
            max_qp: h264_caps.max_qp,
            cabac: h264_caps
                .std_syntax_flags
                .contains(vk::VideoEncodeH264StdFlagsKHR::ENTROPY_CODING_MODE_FLAG_SET),
        })
    }

    fn create_device(
        instance: &ash::Instance,
        pd: vk::PhysicalDevice,
        encode_qf: u32,
        transfer_qf: u32,
        extensions: &[&std::ffi::CStr],
    ) -> Result<ash::Device> {
        let priorities = [1.0f32];
        let mut queue_infos = vec![vk::DeviceQueueCreateInfo::default()
            .queue_family_index(encode_qf)
            .queue_priorities(&priorities)];
        if transfer_qf != encode_qf {
            queue_infos.push(
                vk::DeviceQueueCreateInfo::default()
                    .queue_family_index(transfer_qf)
                    .queue_priorities(&priorities),
            );
        }
        let ext_ptrs: Vec<*const i8> = extensions.iter().map(|e| e.as_ptr()).collect();
        let mut vk13 = vk::PhysicalDeviceVulkan13Features::default().synchronization2(true);
        let device_info = vk::DeviceCreateInfo::default()
            .queue_create_infos(&queue_infos)
            .enabled_extension_names(&ext_ptrs)
            .push_next(&mut vk13);
        unsafe { instance.create_device(pd, &device_info, None) }.map_err(|e| init_err("vkCreateDevice", e))
    }
}

impl VideoEncoder for VulkanVideoEncoder {
    fn name(&self) -> &'static str {
        "Vulkan Video"
    }

    fn capabilities(&self) -> Result<EncoderCapabilities> {
        Ok(EncoderCapabilities {
            name: "Vulkan Video",
            // Only codecs this backend can actually drive today, so
            // negotiation never selects a codec create_session would reject.
            supported_codecs: vec![VideoCodec::H264],
            supports_hdr: false,
            supports_yuv444: false,
            max_resolution: Resolution::new(self.caps.max_coded_extent.width, self.caps.max_coded_extent.height),
            max_framerate: 240,
        })
    }

    fn validate_config(&self, config: &EncodeConfig) -> Result<()> {
        if config.codec != VideoCodec::H264 {
            return Err(FluxError::EncoderInit(format!(
                "Vulkan Video backend currently encodes H.264 only (requested {:?}); \
                 H.265/AV1 land in a later phase",
                config.codec
            )));
        }
        let max = self.caps.max_coded_extent;
        if config.resolution.width > max.width || config.resolution.height > max.height {
            return Err(FluxError::EncoderInit(format!(
                "resolution {} exceeds Vulkan Video maximum {}x{}",
                config.resolution, max.width, max.height
            )));
        }
        if self.caps.max_dpb_slots < 2 || self.caps.max_active_refs < 1 {
            return Err(FluxError::EncoderInit(
                "Vulkan Video: driver DPB too small for IPPP encoding".into(),
            ));
        }
        let rc_mode = match config.rate_control {
            RateControlMode::Cbr => vk::VideoEncodeRateControlModeFlagsKHR::CBR,
            RateControlMode::Vbr => vk::VideoEncodeRateControlModeFlagsKHR::VBR,
            RateControlMode::Cqp => vk::VideoEncodeRateControlModeFlagsKHR::DISABLED,
        };
        if !self.caps.rate_control_modes.contains(rc_mode) {
            return Err(FluxError::EncoderInit(format!(
                "Vulkan Video: driver does not support {:?} rate control",
                config.rate_control
            )));
        }
        Ok(())
    }

    fn create_session(&self, config: EncodeConfig) -> Result<Box<dyn EncodeSession>> {
        self.validate_config(&config)?;
        Ok(Box::new(VulkanVideoSession::new(self.ctx.clone(), self.caps, config)?))
    }
}

impl VulkanVideoEncoder {
    /// Device name reported by the Vulkan driver.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }
}

// ───────────────────────────── session ─────────────────────────────

struct VulkanVideoSession {
    ctx: Arc<VulkanContext>,
    caps: H264Caps,
    config: EncodeConfig,

    video_session: vk::VideoSessionKHR,
    session_memory: Vec<vk::DeviceMemory>,
    session_params: vk::VideoSessionParametersKHR,
    /// Annex-B SPS+PPS produced by the driver, prepended to IDR packets.
    sps_pps: Vec<u8>,

    input_image: vk::Image,
    input_memory: vk::DeviceMemory,
    input_view: vk::ImageView,
    dpb_image: vk::Image,
    dpb_memory: vk::DeviceMemory,
    dpb_views: [vk::ImageView; 2],

    staging_buffer: vk::Buffer,
    staging_memory: vk::DeviceMemory,
    staging_size: vk::DeviceSize,
    bitstream_buffer: vk::Buffer,
    bitstream_memory: vk::DeviceMemory,
    bitstream_size: vk::DeviceSize,

    query_pool: vk::QueryPool,
    transfer_pool: vk::CommandPool,
    transfer_cmd: vk::CommandBuffer,
    encode_pool: vk::CommandPool,
    encode_cmd: vk::CommandBuffer,
    upload_done: vk::Semaphore,
    fence: vk::Fence,

    coded_extent: vk::Extent2D,
    frame_counter: u64,
    /// H.264 `frame_num` counter (wraps at `2^16` per the SPS).
    frame_num: u32,
    idr_pic_id: u16,
    idr_interval: u64,
    idr_requested: bool,
    rc_dirty: bool,
    first_op: bool,
    /// DPB slot the next reconstructed picture is written to (ping-pong).
    setup_slot: usize,
    /// State of the previous frame, which slot `1 - setup_slot` holds.
    prev_frame: Option<h264::FrameState>,
}

// SAFETY: all Vulkan handles are owned by this session and only used from
// `&mut self`; queue submission goes through the context's mutexes.
unsafe impl Send for VulkanVideoSession {}

impl VulkanVideoSession {
    fn new(ctx: Arc<VulkanContext>, caps: H264Caps, config: EncodeConfig) -> Result<Self> {
        tracing::info!(
            "Creating Vulkan Video session: {} {}@{}fps {}kbps",
            config.codec,
            config.resolution,
            config.framerate,
            config.bitrate_kbps,
        );

        let width = config.resolution.width;
        let height = config.resolution.height;
        let coded_extent = vk::Extent2D { width, height };
        let aligned_extent = vk::Extent2D {
            width: h264::mb_count(width) * 16,
            height: h264::mb_count(height) * 16,
        };
        let profile = H264ProfileChain::new();
        let device = &ctx.device;

        // ── video session ──
        let session_info = vk::VideoSessionCreateInfoKHR::default()
            .queue_family_index(ctx.encode_qf)
            .video_profile(profile.profile())
            .picture_format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .max_coded_extent(aligned_extent)
            .reference_picture_format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .max_dpb_slots(2)
            .max_active_reference_pictures(1)
            .std_header_version(&caps.std_header_version);
        let mut video_session = vk::VideoSessionKHR::null();
        unsafe {
            (ctx.video_queue_fn.create_video_session_khr)(
                device.handle(),
                &session_info,
                std::ptr::null(),
                &mut video_session,
            )
        }
        .result()
        .map_err(|e| init_err("vkCreateVideoSessionKHR", e))?;

        let mut guard = SessionGuard::new(ctx.clone(), video_session);

        // ── session memory ──
        let mut req_count = 0u32;
        unsafe {
            (ctx.video_queue_fn.get_video_session_memory_requirements_khr)(
                device.handle(),
                video_session,
                &mut req_count,
                std::ptr::null_mut(),
            )
        }
        .result()
        .map_err(|e| init_err("vkGetVideoSessionMemoryRequirementsKHR", e))?;
        let mut reqs = vec![vk::VideoSessionMemoryRequirementsKHR::default(); req_count as usize];
        unsafe {
            (ctx.video_queue_fn.get_video_session_memory_requirements_khr)(
                device.handle(),
                video_session,
                &mut req_count,
                reqs.as_mut_ptr(),
            )
        }
        .result()
        .map_err(|e| init_err("vkGetVideoSessionMemoryRequirementsKHR", e))?;

        let mut binds = Vec::with_capacity(reqs.len());
        for req in &reqs {
            let mem_type = ctx
                .find_memory_type(
                    req.memory_requirements.memory_type_bits,
                    vk::MemoryPropertyFlags::DEVICE_LOCAL,
                )
                .or_else(|_| {
                    ctx.find_memory_type(
                        req.memory_requirements.memory_type_bits,
                        vk::MemoryPropertyFlags::empty(),
                    )
                })?;
            let alloc = vk::MemoryAllocateInfo::default()
                .allocation_size(req.memory_requirements.size)
                .memory_type_index(mem_type);
            let memory = unsafe { device.allocate_memory(&alloc, None) }
                .map_err(|e| init_err("vkAllocateMemory (session)", e))?;
            guard.memories.push(memory);
            binds.push(
                vk::BindVideoSessionMemoryInfoKHR::default()
                    .memory_bind_index(req.memory_bind_index)
                    .memory(memory)
                    .memory_offset(0)
                    .memory_size(req.memory_requirements.size),
            );
        }
        unsafe {
            (ctx.video_queue_fn.bind_video_session_memory_khr)(
                device.handle(),
                video_session,
                binds.len() as u32,
                binds.as_ptr(),
            )
        }
        .result()
        .map_err(|e| init_err("vkBindVideoSessionMemoryKHR", e))?;

        // ── session parameters (SPS/PPS) ──
        let sps = [h264::build_sps(width, height, config.framerate)];
        let pps = [h264::build_pps(caps.cabac)];
        let add_info = vk::VideoEncodeH264SessionParametersAddInfoKHR::default()
            .std_sp_ss(&sps)
            .std_pp_ss(&pps);
        let mut h264_params_info = vk::VideoEncodeH264SessionParametersCreateInfoKHR::default()
            .max_std_sps_count(1)
            .max_std_pps_count(1)
            .parameters_add_info(&add_info);
        let params_info = vk::VideoSessionParametersCreateInfoKHR::default()
            .video_session(video_session)
            .push_next(&mut h264_params_info);
        let mut session_params = vk::VideoSessionParametersKHR::null();
        unsafe {
            (ctx.video_queue_fn.create_video_session_parameters_khr)(
                device.handle(),
                &params_info,
                std::ptr::null(),
                &mut session_params,
            )
        }
        .result()
        .map_err(|e| init_err("vkCreateVideoSessionParametersKHR", e))?;
        guard.params = session_params;

        // ── encoded SPS/PPS bytes ──
        let mut h264_get = vk::VideoEncodeH264SessionParametersGetInfoKHR::default()
            .write_std_sps(true)
            .write_std_pps(true)
            .std_sps_id(0)
            .std_pps_id(0);
        let get_info = vk::VideoEncodeSessionParametersGetInfoKHR::default()
            .video_session_parameters(session_params)
            .push_next(&mut h264_get);
        let mut data_size = 0usize;
        unsafe {
            (ctx.encode_queue_fn.get_encoded_video_session_parameters_khr)(
                device.handle(),
                &get_info,
                std::ptr::null_mut(),
                &mut data_size,
                std::ptr::null_mut(),
            )
        }
        .result()
        .map_err(|e| init_err("vkGetEncodedVideoSessionParametersKHR", e))?;
        let mut sps_pps = vec![0u8; data_size];
        unsafe {
            (ctx.encode_queue_fn.get_encoded_video_session_parameters_khr)(
                device.handle(),
                &get_info,
                std::ptr::null_mut(),
                &mut data_size,
                sps_pps.as_mut_ptr() as *mut c_void,
            )
        }
        .result()
        .map_err(|e| init_err("vkGetEncodedVideoSessionParametersKHR", e))?;
        sps_pps.truncate(data_size);

        // ── images ──
        let qf_indices = [ctx.encode_qf, ctx.transfer_qf];
        let concurrent = ctx.encode_qf != ctx.transfer_qf;

        let mut input_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .extent(vk::Extent3D {
                width: aligned_extent.width,
                height: aligned_extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR | vk::ImageUsageFlags::TRANSFER_DST)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        if concurrent {
            input_info = input_info
                .sharing_mode(vk::SharingMode::CONCURRENT)
                .queue_family_indices(&qf_indices);
        }
        input_info.p_next = profile.list_ptr();
        let (input_image, input_memory) = create_image(&ctx, &input_info)?;
        guard.images.push((input_image, input_memory));

        let mut dpb_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .extent(vk::Extent3D {
                width: aligned_extent.width,
                height: aligned_extent.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(2)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        dpb_info.p_next = profile.list_ptr();
        let (dpb_image, dpb_memory) = create_image(&ctx, &dpb_info)?;
        guard.images.push((dpb_image, dpb_memory));

        let input_view = create_view(&ctx, input_image, 0)?;
        guard.views.push(input_view);
        let dpb_views = [create_view(&ctx, dpb_image, 0)?, create_view(&ctx, dpb_image, 1)?];
        guard.views.push(dpb_views[0]);
        guard.views.push(dpb_views[1]);

        // ── buffers ──
        let staging_size = (width as vk::DeviceSize) * (height as vk::DeviceSize) * 3 / 2;
        let staging_info = vk::BufferCreateInfo::default()
            .size(staging_size)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let (staging_buffer, staging_memory) = create_buffer(
            &ctx,
            &staging_info,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        guard.buffers.push((staging_buffer, staging_memory));

        let bitstream_size = align_up(
            MIN_BITSTREAM_BUFFER_SIZE.max(staging_size),
            caps.min_bitstream_size_align.max(1),
        );
        let mut bitstream_info = vk::BufferCreateInfo::default()
            .size(bitstream_size)
            .usage(vk::BufferUsageFlags::VIDEO_ENCODE_DST_KHR)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        bitstream_info.p_next = profile.list_ptr();
        let (bitstream_buffer, bitstream_memory) = create_buffer(
            &ctx,
            &bitstream_info,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        guard.buffers.push((bitstream_buffer, bitstream_memory));

        // ── query pool, command pools, sync ──
        let mut feedback_info = vk::QueryPoolVideoEncodeFeedbackCreateInfoKHR::default().encode_feedback_flags(
            vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BUFFER_OFFSET
                | vk::VideoEncodeFeedbackFlagsKHR::BITSTREAM_BYTES_WRITTEN,
        );
        feedback_info.p_next = profile.profile() as *const _ as *const c_void;
        let mut query_info = vk::QueryPoolCreateInfo::default()
            .query_type(vk::QueryType::VIDEO_ENCODE_FEEDBACK_KHR)
            .query_count(1);
        query_info.p_next = &feedback_info as *const _ as *const c_void;
        let query_pool =
            unsafe { device.create_query_pool(&query_info, None) }.map_err(|e| init_err("vkCreateQueryPool", e))?;
        guard.query_pool = query_pool;

        let (transfer_pool, transfer_cmd) = create_pool_and_cmd(&ctx, ctx.transfer_qf)?;
        guard.pools.push(transfer_pool);
        let (encode_pool, encode_cmd) = create_pool_and_cmd(&ctx, ctx.encode_qf)?;
        guard.pools.push(encode_pool);

        let upload_done = unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }
            .map_err(|e| init_err("vkCreateSemaphore", e))?;
        guard.semaphore = upload_done;
        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }
            .map_err(|e| init_err("vkCreateFence", e))?;
        guard.fence = fence;

        let idr_interval = match config.gop_size {
            0 => INFINITE_GOP_IDR_INTERVAL,
            n => u64::from(n),
        };

        guard.disarm();
        Ok(Self {
            ctx,
            caps,
            config,
            video_session,
            session_memory: binds.iter().map(|b| b.memory).collect(),
            session_params,
            sps_pps,
            input_image,
            input_memory,
            input_view,
            dpb_image,
            dpb_memory,
            dpb_views,
            staging_buffer,
            staging_memory,
            staging_size,
            bitstream_buffer,
            bitstream_memory,
            bitstream_size,
            query_pool,
            transfer_pool,
            transfer_cmd,
            encode_pool,
            encode_cmd,
            upload_done,
            fence,
            coded_extent,
            frame_counter: 0,
            frame_num: 0,
            idr_pic_id: 0,
            idr_interval,
            idr_requested: true,
            rc_dirty: true,
            first_op: true,
            setup_slot: 0,
            prev_frame: None,
        })
    }

    /// Upload the NV12 frame into the staging buffer and record + submit the
    /// buffer→image copy on the transfer queue.
    fn upload_input(&mut self, nv12: &[u8], frame_seq: u64) -> Result<()> {
        let device = &self.ctx.device;
        let enc_err = |what: &str, e: vk::Result| FluxError::Encode {
            frame: frame_seq,
            reason: format!("Vulkan Video: {what} failed: {e:?}"),
        };

        unsafe {
            let ptr = device
                .map_memory(self.staging_memory, 0, self.staging_size, vk::MemoryMapFlags::empty())
                .map_err(|e| enc_err("vkMapMemory (staging)", e))?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr as *mut u8, nv12.len());
            device.unmap_memory(self.staging_memory);
        }

        let width = self.coded_extent.width;
        let height = self.coded_extent.height;
        unsafe {
            device
                .begin_command_buffer(
                    self.transfer_cmd,
                    &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|e| enc_err("vkBeginCommandBuffer", e))?;

            let to_transfer = vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::COPY)
                .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .image(self.input_image)
                .subresource_range(full_range(1));
            device.cmd_pipeline_barrier2(
                self.transfer_cmd,
                &vk::DependencyInfo::default().image_memory_barriers(std::slice::from_ref(&to_transfer)),
            );

            let regions = [
                vk::BufferImageCopy {
                    buffer_offset: 0,
                    buffer_row_length: 0,
                    buffer_image_height: 0,
                    image_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::PLANE_0,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_offset: vk::Offset3D::default(),
                    image_extent: vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    },
                },
                vk::BufferImageCopy {
                    buffer_offset: vk::DeviceSize::from(width) * vk::DeviceSize::from(height),
                    buffer_row_length: 0,
                    buffer_image_height: 0,
                    image_subresource: vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::PLANE_1,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    },
                    image_offset: vk::Offset3D::default(),
                    image_extent: vk::Extent3D {
                        width: width / 2,
                        height: height / 2,
                        depth: 1,
                    },
                },
            ];
            device.cmd_copy_buffer_to_image(
                self.transfer_cmd,
                self.staging_buffer,
                self.input_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &regions,
            );

            device
                .end_command_buffer(self.transfer_cmd)
                .map_err(|e| enc_err("vkEndCommandBuffer", e))?;

            let signal = [self.upload_done];
            let cmds = [self.transfer_cmd];
            let submit = vk::SubmitInfo::default()
                .command_buffers(&cmds)
                .signal_semaphores(&signal);
            let queue = self.ctx.transfer_queue.lock();
            device
                .queue_submit(*queue, &[submit], vk::Fence::null())
                .map_err(|e| enc_err("vkQueueSubmit (transfer)", e))?;
        }
        Ok(())
    }

    /// Record the rate-control control command (applied on session reset and
    /// after `set_bitrate`).
    fn record_rate_control(&self, cmd: vk::CommandBuffer, reset: bool) {
        let gop = u32::try_from(self.idr_interval).unwrap_or(u32::MAX);
        let h264_rc = vk::VideoEncodeH264RateControlInfoKHR::default()
            .flags(
                vk::VideoEncodeH264RateControlFlagsKHR::REGULAR_GOP
                    | vk::VideoEncodeH264RateControlFlagsKHR::REFERENCE_PATTERN_FLAT,
            )
            .gop_frame_count(gop)
            .idr_period(gop)
            .consecutive_b_frame_count(0)
            .temporal_layer_count(1);

        let avg_bitrate = u64::from(self.config.bitrate_kbps) * 1000;
        let framerate = self.config.framerate.max(1);
        let h264_layer = vk::VideoEncodeH264RateControlLayerInfoKHR::default();
        let mut layer = vk::VideoEncodeRateControlLayerInfoKHR::default()
            .average_bitrate(avg_bitrate)
            .max_bitrate(match self.config.rate_control {
                RateControlMode::Vbr => avg_bitrate * 3 / 2,
                _ => avg_bitrate,
            })
            .frame_rate_numerator(framerate)
            .frame_rate_denominator(1);
        layer.p_next = &h264_layer as *const _ as *const c_void;

        let layers = [layer];
        let mode = match self.config.rate_control {
            RateControlMode::Cbr => vk::VideoEncodeRateControlModeFlagsKHR::CBR,
            RateControlMode::Vbr => vk::VideoEncodeRateControlModeFlagsKHR::VBR,
            RateControlMode::Cqp => vk::VideoEncodeRateControlModeFlagsKHR::DISABLED,
        };
        let mut rc_info = vk::VideoEncodeRateControlInfoKHR::default()
            .rate_control_mode(mode)
            .virtual_buffer_size_in_ms(1000)
            .initial_virtual_buffer_size_in_ms(500);
        rc_info.p_next = &h264_rc as *const _ as *const c_void;
        if mode != vk::VideoEncodeRateControlModeFlagsKHR::DISABLED {
            rc_info = rc_info.layers(&layers);
        }

        let mut flags = vk::VideoCodingControlFlagsKHR::ENCODE_RATE_CONTROL;
        if reset {
            flags |= vk::VideoCodingControlFlagsKHR::RESET;
        }
        let mut control = vk::VideoCodingControlInfoKHR::default().flags(flags);
        control.p_next = &rc_info as *const _ as *const c_void;
        unsafe { (self.ctx.video_queue_fn.cmd_control_video_coding_khr)(cmd, &control) };
    }
}

impl EncodeSession for VulkanVideoSession {
    fn encode(&mut self, frame: &CapturedFrame) -> Result<Vec<EncodedPacket>> {
        let width = self.coded_extent.width;
        let height = self.coded_extent.height;
        let nv12 = frame_to_nv12(frame, width, height)?;
        let enc_err = |what: &str, e: vk::Result| FluxError::Encode {
            frame: frame.sequence,
            reason: format!("Vulkan Video: {what} failed: {e:?}"),
        };

        let is_idr =
            self.idr_requested || self.prev_frame.is_none() || self.frame_counter.is_multiple_of(self.idr_interval);
        if is_idr {
            self.frame_num = 0;
        }
        let state = h264::FrameState {
            is_idr,
            frame_num: self.frame_num,
            pic_order_cnt: (self.frame_num as i32).wrapping_mul(2),
            idr_pic_id: self.idr_pic_id,
        };

        self.upload_input(&nv12, frame.sequence)?;

        let device = &self.ctx.device;
        let cmd = self.encode_cmd;
        let setup_slot = self.setup_slot;
        let ref_slot = 1 - setup_slot;

        unsafe {
            device
                .begin_command_buffer(
                    cmd,
                    &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|e| enc_err("vkBeginCommandBuffer", e))?;

            // Input image: copy result → encode source layout.
            let mut barriers = vec![vk::ImageMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::NONE)
                .src_access_mask(vk::AccessFlags2::NONE)
                .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                .dst_access_mask(vk::AccessFlags2::VIDEO_ENCODE_READ_KHR)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::VIDEO_ENCODE_SRC_KHR)
                .image(self.input_image)
                .subresource_range(full_range(1))];
            if self.first_op {
                barriers.push(
                    vk::ImageMemoryBarrier2::default()
                        .src_stage_mask(vk::PipelineStageFlags2::NONE)
                        .src_access_mask(vk::AccessFlags2::NONE)
                        .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                        .dst_access_mask(
                            vk::AccessFlags2::VIDEO_ENCODE_READ_KHR | vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR,
                        )
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::VIDEO_ENCODE_DPB_KHR)
                        .image(self.dpb_image)
                        .subresource_range(full_range(2)),
                );
            }
            device.cmd_pipeline_barrier2(cmd, &vk::DependencyInfo::default().image_memory_barriers(&barriers));

            device.cmd_reset_query_pool(cmd, self.query_pool, 0, 1);

            // ── begin video coding scope ──
            let setup_resource = vk::VideoPictureResourceInfoKHR::default()
                .coded_extent(self.coded_extent)
                .image_view_binding(self.dpb_views[setup_slot]);
            let ref_resource = vk::VideoPictureResourceInfoKHR::default()
                .coded_extent(self.coded_extent)
                .image_view_binding(self.dpb_views[ref_slot]);

            let prev_state = self.prev_frame.unwrap_or(state);
            let prev_ref_info = h264::build_reference_info(&prev_state);
            let mut prev_dpb_info = vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(&prev_ref_info);

            let mut begin_slots = vec![vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(-1)
                .picture_resource(&setup_resource)];
            if !is_idr {
                begin_slots.push(
                    vk::VideoReferenceSlotInfoKHR::default()
                        .slot_index(ref_slot as i32)
                        .picture_resource(&ref_resource)
                        .push_next(&mut prev_dpb_info),
                );
            }
            let begin_info = vk::VideoBeginCodingInfoKHR::default()
                .video_session(self.video_session)
                .video_session_parameters(self.session_params)
                .reference_slots(&begin_slots);
            (self.ctx.video_queue_fn.cmd_begin_video_coding_khr)(cmd, &begin_info);

            if self.first_op || self.rc_dirty {
                self.record_rate_control(cmd, self.first_op);
            }

            // ── encode ──
            let setup_ref_info = h264::build_reference_info(&state);
            let mut setup_dpb_info = vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(&setup_ref_info);
            let setup_slot_info = vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(setup_slot as i32)
                .picture_resource(&setup_resource)
                .push_next(&mut setup_dpb_info);

            let ref_lists = h264::build_reference_lists(ref_slot as u8);
            let std_pic_info = h264::build_picture_info(&state, (!is_idr).then_some(&ref_lists));

            let qp_delta = (CONSTANT_QP.clamp(self.caps.min_qp, self.caps.max_qp) - 26) as i8;
            let slice_header = h264::build_slice_header(
                is_idr,
                if self.config.rate_control == RateControlMode::Cqp {
                    qp_delta
                } else {
                    0
                },
            );
            let slice = vk::VideoEncodeH264NaluSliceInfoKHR::default()
                .constant_qp(if self.config.rate_control == RateControlMode::Cqp {
                    CONSTANT_QP.clamp(self.caps.min_qp, self.caps.max_qp)
                } else {
                    0
                })
                .std_slice_header(&slice_header);
            let slices = [slice];
            let mut h264_pic_info = vk::VideoEncodeH264PictureInfoKHR::default()
                .nalu_slice_entries(&slices)
                .std_picture_info(&std_pic_info);

            let mut ref_dpb_info = vk::VideoEncodeH264DpbSlotInfoKHR::default().std_reference_info(&prev_ref_info);
            let ref_slots;
            let src_resource = vk::VideoPictureResourceInfoKHR::default()
                .coded_extent(self.coded_extent)
                .image_view_binding(self.input_view);
            let mut encode_info = vk::VideoEncodeInfoKHR::default()
                .dst_buffer(self.bitstream_buffer)
                .dst_buffer_offset(0)
                .dst_buffer_range(self.bitstream_size)
                .src_picture_resource(src_resource)
                .setup_reference_slot(&setup_slot_info)
                .push_next(&mut h264_pic_info);
            if !is_idr {
                ref_slots = [vk::VideoReferenceSlotInfoKHR::default()
                    .slot_index(ref_slot as i32)
                    .picture_resource(&ref_resource)
                    .push_next(&mut ref_dpb_info)];
                encode_info = encode_info.reference_slots(&ref_slots);
            }

            device.cmd_begin_query(cmd, self.query_pool, 0, vk::QueryControlFlags::empty());
            (self.ctx.encode_queue_fn.cmd_encode_video_khr)(cmd, &encode_info);
            device.cmd_end_query(cmd, self.query_pool, 0);

            (self.ctx.video_queue_fn.cmd_end_video_coding_khr)(cmd, &vk::VideoEndCodingInfoKHR::default());

            // Make the bitstream visible to the host.
            let buffer_barrier = vk::BufferMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::VIDEO_ENCODE_KHR)
                .src_access_mask(vk::AccessFlags2::VIDEO_ENCODE_WRITE_KHR)
                .dst_stage_mask(vk::PipelineStageFlags2::HOST)
                .dst_access_mask(vk::AccessFlags2::HOST_READ)
                .buffer(self.bitstream_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE);
            device.cmd_pipeline_barrier2(
                cmd,
                &vk::DependencyInfo::default().buffer_memory_barriers(std::slice::from_ref(&buffer_barrier)),
            );

            device
                .end_command_buffer(cmd)
                .map_err(|e| enc_err("vkEndCommandBuffer", e))?;

            let waits = [self.upload_done];
            let wait_stages = [vk::PipelineStageFlags::ALL_COMMANDS];
            let cmds = [cmd];
            let submit = vk::SubmitInfo::default()
                .wait_semaphores(&waits)
                .wait_dst_stage_mask(&wait_stages)
                .command_buffers(&cmds);
            {
                let queue = self.ctx.encode_queue.lock();
                device
                    .queue_submit(*queue, &[submit], self.fence)
                    .map_err(|e| enc_err("vkQueueSubmit (encode)", e))?;
            }

            device
                .wait_for_fences(&[self.fence], true, u64::MAX)
                .map_err(|e| enc_err("vkWaitForFences", e))?;
            device
                .reset_fences(&[self.fence])
                .map_err(|e| enc_err("vkResetFences", e))?;
        }

        // ── read back ──
        let mut feedback = [[0u32; 2]; 1];
        unsafe { device.get_query_pool_results(self.query_pool, 0, &mut feedback, vk::QueryResultFlags::WAIT) }
            .map_err(|e| enc_err("vkGetQueryPoolResults", e))?;
        let (offset, bytes) = (feedback[0][0] as usize, feedback[0][1] as usize);

        let mut data = Vec::with_capacity(self.sps_pps.len() + bytes);
        if is_idr {
            data.extend_from_slice(&self.sps_pps);
        }
        unsafe {
            let ptr = device
                .map_memory(self.bitstream_memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
                .map_err(|e| enc_err("vkMapMemory (bitstream)", e))?;
            data.extend_from_slice(std::slice::from_raw_parts((ptr as *const u8).add(offset), bytes));
            device.unmap_memory(self.bitstream_memory);
        }

        // Reset the recorded command buffers for the next frame.
        unsafe {
            device
                .reset_command_buffer(self.transfer_cmd, vk::CommandBufferResetFlags::empty())
                .and_then(|_| device.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty()))
                .map_err(|e| enc_err("vkResetCommandBuffer", e))?;
        }

        // ── advance state ──
        self.frame_counter += 1;
        self.frame_num = (self.frame_num + 1) % (1u32 << (u32::from(h264::LOG2_MAX_FRAME_NUM_MINUS4) + 4));
        if is_idr {
            self.idr_pic_id = self.idr_pic_id.wrapping_add(1);
        }
        self.idr_requested = false;
        self.rc_dirty = false;
        self.first_op = false;
        self.setup_slot = ref_slot;
        self.prev_frame = Some(state);

        Ok(vec![EncodedPacket {
            frame_index: self.frame_counter,
            pts: frame.sequence,
            is_keyframe: is_idr,
            data,
        }])
    }

    fn request_idr(&mut self) {
        tracing::debug!("Vulkan Video: IDR frame requested");
        self.idr_requested = true;
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        // Synchronous IPPP encoding buffers nothing.
        Ok(vec![])
    }

    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()> {
        tracing::info!("Vulkan Video: bitrate updated to {} kbps", bitrate_kbps);
        self.config.bitrate_kbps = bitrate_kbps;
        self.rc_dirty = true;
        Ok(())
    }
}

impl Drop for VulkanVideoSession {
    fn drop(&mut self) {
        let device = &self.ctx.device;
        unsafe {
            let _ = device.device_wait_idle();
            device.destroy_fence(self.fence, None);
            device.destroy_semaphore(self.upload_done, None);
            device.destroy_command_pool(self.encode_pool, None);
            device.destroy_command_pool(self.transfer_pool, None);
            device.destroy_query_pool(self.query_pool, None);
            device.destroy_buffer(self.bitstream_buffer, None);
            device.free_memory(self.bitstream_memory, None);
            device.destroy_buffer(self.staging_buffer, None);
            device.free_memory(self.staging_memory, None);
            for view in self.dpb_views {
                device.destroy_image_view(view, None);
            }
            device.destroy_image_view(self.input_view, None);
            device.destroy_image(self.dpb_image, None);
            device.free_memory(self.dpb_memory, None);
            device.destroy_image(self.input_image, None);
            device.free_memory(self.input_memory, None);
            (self.ctx.video_queue_fn.destroy_video_session_parameters_khr)(
                device.handle(),
                self.session_params,
                std::ptr::null(),
            );
            for mem in &self.session_memory {
                device.free_memory(*mem, None);
            }
            (self.ctx.video_queue_fn.destroy_video_session_khr)(device.handle(), self.video_session, std::ptr::null());
        }
    }
}

// ───────────────────────────── helpers ─────────────────────────────

fn align_up(v: vk::DeviceSize, align: vk::DeviceSize) -> vk::DeviceSize {
    v.div_ceil(align) * align
}

fn full_range(layers: u32) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: layers,
    }
}

fn create_image(ctx: &VulkanContext, info: &vk::ImageCreateInfo) -> Result<(vk::Image, vk::DeviceMemory)> {
    let device = &ctx.device;
    let image = unsafe { device.create_image(info, None) }.map_err(|e| init_err("vkCreateImage", e))?;
    let reqs = unsafe { device.get_image_memory_requirements(image) };
    let mem_type = match ctx.find_memory_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL) {
        Ok(t) => t,
        Err(e) => {
            unsafe { device.destroy_image(image, None) };
            return Err(e);
        }
    };
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(reqs.size)
        .memory_type_index(mem_type);
    let memory = match unsafe { device.allocate_memory(&alloc, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { device.destroy_image(image, None) };
            return Err(init_err("vkAllocateMemory (image)", e));
        }
    };
    if let Err(e) = unsafe { device.bind_image_memory(image, memory, 0) } {
        unsafe {
            device.destroy_image(image, None);
            device.free_memory(memory, None);
        }
        return Err(init_err("vkBindImageMemory", e));
    }
    Ok((image, memory))
}

fn create_view(ctx: &VulkanContext, image: vk::Image, layer: u32) -> Result<vk::ImageView> {
    let info = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
        .subresource_range(vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: layer,
            layer_count: 1,
        });
    unsafe { ctx.device.create_image_view(&info, None) }.map_err(|e| init_err("vkCreateImageView", e))
}

fn create_buffer(
    ctx: &VulkanContext,
    info: &vk::BufferCreateInfo,
    props: vk::MemoryPropertyFlags,
) -> Result<(vk::Buffer, vk::DeviceMemory)> {
    let device = &ctx.device;
    let buffer = unsafe { device.create_buffer(info, None) }.map_err(|e| init_err("vkCreateBuffer", e))?;
    let reqs = unsafe { device.get_buffer_memory_requirements(buffer) };
    let mem_type = match ctx.find_memory_type(reqs.memory_type_bits, props) {
        Ok(t) => t,
        Err(e) => {
            unsafe { device.destroy_buffer(buffer, None) };
            return Err(e);
        }
    };
    let alloc = vk::MemoryAllocateInfo::default()
        .allocation_size(reqs.size)
        .memory_type_index(mem_type);
    let memory = match unsafe { device.allocate_memory(&alloc, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { device.destroy_buffer(buffer, None) };
            return Err(init_err("vkAllocateMemory (buffer)", e));
        }
    };
    if let Err(e) = unsafe { device.bind_buffer_memory(buffer, memory, 0) } {
        unsafe {
            device.destroy_buffer(buffer, None);
            device.free_memory(memory, None);
        }
        return Err(init_err("vkBindBufferMemory", e));
    }
    Ok((buffer, memory))
}

fn create_pool_and_cmd(ctx: &VulkanContext, qf: u32) -> Result<(vk::CommandPool, vk::CommandBuffer)> {
    let device = &ctx.device;
    let pool_info = vk::CommandPoolCreateInfo::default()
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
        .queue_family_index(qf);
    let pool =
        unsafe { device.create_command_pool(&pool_info, None) }.map_err(|e| init_err("vkCreateCommandPool", e))?;
    let alloc = vk::CommandBufferAllocateInfo::default()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    match unsafe { device.allocate_command_buffers(&alloc) } {
        Ok(cmds) => Ok((pool, cmds[0])),
        Err(e) => {
            unsafe { device.destroy_command_pool(pool, None) };
            Err(init_err("vkAllocateCommandBuffers", e))
        }
    }
}

/// Destroys partially-created session resources if `Session::new` errors out
/// before completing; disarmed on success (the session's `Drop` then owns them).
struct SessionGuard {
    ctx: Arc<VulkanContext>,
    session: vk::VideoSessionKHR,
    params: vk::VideoSessionParametersKHR,
    memories: Vec<vk::DeviceMemory>,
    images: Vec<(vk::Image, vk::DeviceMemory)>,
    views: Vec<vk::ImageView>,
    buffers: Vec<(vk::Buffer, vk::DeviceMemory)>,
    query_pool: vk::QueryPool,
    pools: Vec<vk::CommandPool>,
    semaphore: vk::Semaphore,
    fence: vk::Fence,
    armed: bool,
}

impl SessionGuard {
    fn new(ctx: Arc<VulkanContext>, session: vk::VideoSessionKHR) -> Self {
        Self {
            ctx,
            session,
            params: vk::VideoSessionParametersKHR::null(),
            memories: Vec::new(),
            images: Vec::new(),
            views: Vec::new(),
            buffers: Vec::new(),
            query_pool: vk::QueryPool::null(),
            pools: Vec::new(),
            semaphore: vk::Semaphore::null(),
            fence: vk::Fence::null(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let device = &self.ctx.device;
        unsafe {
            if self.fence != vk::Fence::null() {
                device.destroy_fence(self.fence, None);
            }
            if self.semaphore != vk::Semaphore::null() {
                device.destroy_semaphore(self.semaphore, None);
            }
            for pool in &self.pools {
                device.destroy_command_pool(*pool, None);
            }
            if self.query_pool != vk::QueryPool::null() {
                device.destroy_query_pool(self.query_pool, None);
            }
            for (buffer, memory) in &self.buffers {
                device.destroy_buffer(*buffer, None);
                device.free_memory(*memory, None);
            }
            for view in &self.views {
                device.destroy_image_view(*view, None);
            }
            for (image, memory) in &self.images {
                device.destroy_image(*image, None);
                device.free_memory(*memory, None);
            }
            if self.params != vk::VideoSessionParametersKHR::null() {
                (self.ctx.video_queue_fn.destroy_video_session_parameters_khr)(
                    device.handle(),
                    self.params,
                    std::ptr::null(),
                );
            }
            for memory in &self.memories {
                device.free_memory(*memory, None);
            }
            (self.ctx.video_queue_fn.destroy_video_session_khr)(device.handle(), self.session, std::ptr::null());
        }
    }
}
