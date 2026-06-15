//! GPU frame import abstraction.
//!
//! Decouples *how a captured frame is made available to the encoder* from the
//! encoder itself. A captured frame may be GPU-resident (DMA-BUF, imported
//! with zero copy) or CPU-resident (shared memory, uploaded to a GPU surface).
//! Each hardware backend implements [`GpuFrameImport`] over its own surface
//! type (e.g. a VA-API `VASurfaceID`).

use flux_core::error::{FluxError, Result};
use flux_core::frame::{CapturedFrame, DmaBufHandle, GpuFrameHandle};

/// Which import path a given [`CapturedFrame`] requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportPath {
    /// Zero-copy import of a DMA-BUF handle.
    DmaBuf,
    /// CPU upload of `CapturedFrame::data`.
    CpuUpload,
}

/// Decide how a frame should be brought onto the GPU.
///
/// DMA-BUF is used when the frame carries a Linux DMA-BUF handle with at least
/// one plane; otherwise the CPU `data` buffer is uploaded.
pub fn classify_frame(frame: &CapturedFrame) -> Result<ImportPath> {
    match &frame.gpu_handle {
        Some(GpuFrameHandle::DmaBuf(h)) if !h.planes.is_empty() => Ok(ImportPath::DmaBuf),
        Some(GpuFrameHandle::DmaBuf(_)) => Err(FluxError::Gpu("DMA-BUF handle has no planes".into())),
        // A DXGI handle reaching a Linux import is a programming error.
        Some(GpuFrameHandle::DxgiSharedTexture(_)) => {
            Err(FluxError::Gpu("DXGI shared texture cannot be imported on Linux".into()))
        }
        None if !frame.data.is_empty() => Ok(ImportPath::CpuUpload),
        None => Err(FluxError::Gpu(
            "frame has neither a GPU handle nor CPU pixel data".into(),
        )),
    }
}

/// Imports captured frames onto the GPU as encoder-ready surfaces.
pub trait GpuFrameImport {
    /// Backend-specific GPU surface type (e.g. a VA-API `VASurfaceID`).
    type Surface;

    /// Import a DMA-BUF handle as a GPU surface with no CPU copy.
    fn import_dmabuf(&mut self, handle: &DmaBufHandle) -> Result<Self::Surface>;

    /// Upload a CPU frame's pixel data into a GPU surface.
    fn upload_cpu(&mut self, frame: &CapturedFrame) -> Result<Self::Surface>;

    /// Import a frame via whichever path it requires (see [`classify_frame`]).
    fn import(&mut self, frame: &CapturedFrame) -> Result<Self::Surface> {
        match classify_frame(frame)? {
            ImportPath::DmaBuf => match &frame.gpu_handle {
                Some(GpuFrameHandle::DmaBuf(h)) => self.import_dmabuf(h),
                _ => unreachable!("classify_frame guarantees a DMA-BUF handle"),
            },
            ImportPath::CpuUpload => self.upload_cpu(frame),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::frame::{DmaBufHandle, DmaBufPlane};
    use flux_core::types::{PixelFormat, Resolution};
    use std::os::fd::OwnedFd;
    use std::sync::Arc;
    use std::time::Instant;

    fn owned_fd() -> Arc<OwnedFd> {
        // A pipe read-end is a cheap, valid owned fd for tests.
        let (r, _w) = std::io::pipe().unwrap();
        Arc::new(OwnedFd::from(r))
    }

    fn base_frame() -> CapturedFrame {
        CapturedFrame {
            sequence: 1,
            timestamp: Instant::now(),
            format: PixelFormat::Bgra8,
            resolution: Resolution::new(16, 16),
            stride: 64,
            data: Vec::new(),
            gpu_handle: None,
        }
    }

    #[test]
    fn classify_dmabuf_frame() {
        let mut f = base_frame();
        f.gpu_handle = Some(GpuFrameHandle::DmaBuf(DmaBufHandle {
            planes: vec![DmaBufPlane {
                fd: owned_fd(),
                offset: 0,
                stride: 64,
            }],
            modifier: 0,
            fourcc: 0,
            width: 16,
            height: 16,
        }));
        assert_eq!(classify_frame(&f).unwrap(), ImportPath::DmaBuf);
    }

    #[test]
    fn classify_cpu_frame() {
        let mut f = base_frame();
        f.data = vec![0u8; 1024];
        assert_eq!(classify_frame(&f).unwrap(), ImportPath::CpuUpload);
    }

    #[test]
    fn classify_empty_frame_errors() {
        assert!(classify_frame(&base_frame()).is_err());
    }

    #[test]
    fn classify_planeless_dmabuf_errors() {
        let mut f = base_frame();
        f.gpu_handle = Some(GpuFrameHandle::DmaBuf(DmaBufHandle {
            planes: Vec::new(),
            modifier: 0,
            fourcc: 0,
            width: 16,
            height: 16,
        }));
        assert!(classify_frame(&f).is_err());
    }

    /// A trivial importer used to verify `import` dispatches on the path.
    struct CountingImport {
        dmabuf: u32,
        cpu: u32,
    }
    impl GpuFrameImport for CountingImport {
        type Surface = ImportPath;
        fn import_dmabuf(&mut self, _h: &DmaBufHandle) -> Result<ImportPath> {
            self.dmabuf += 1;
            Ok(ImportPath::DmaBuf)
        }
        fn upload_cpu(&mut self, _f: &CapturedFrame) -> Result<ImportPath> {
            self.cpu += 1;
            Ok(ImportPath::CpuUpload)
        }
    }

    #[test]
    fn import_dispatches_on_path() {
        let mut imp = CountingImport { dmabuf: 0, cpu: 0 };
        let mut cpu = base_frame();
        cpu.data = vec![0u8; 8];
        assert_eq!(imp.import(&cpu).unwrap(), ImportPath::CpuUpload);

        let mut dma = base_frame();
        dma.gpu_handle = Some(GpuFrameHandle::DmaBuf(DmaBufHandle {
            planes: vec![DmaBufPlane {
                fd: owned_fd(),
                offset: 0,
                stride: 64,
            }],
            modifier: 0,
            fourcc: 0,
            width: 16,
            height: 16,
        }));
        assert_eq!(imp.import(&dma).unwrap(), ImportPath::DmaBuf);
        assert_eq!((imp.dmabuf, imp.cpu), (1, 1));
    }
}
