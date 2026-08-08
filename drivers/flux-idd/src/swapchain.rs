//! Swap-chain processor: consumes the frames the OS renders for the virtual
//! monitor on a dedicated thread. Frames are acquired and immediately
//! released — capture happens through DXGI Desktop Duplication in
//! flux-server, not here.

use core::ffi::c_void;
use std::sync::mpsc;
use std::thread::JoinHandle;

use windows::core::Interface;
use windows::Win32::Foundation::{HANDLE, HMODULE, LUID, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, IDXGIAdapter1, IDXGIDevice, IDXGIFactory5,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects};

use crate::bindings as idd;

const E_PENDING: i32 = 0x8000000Au32 as i32;

struct D3DDevice {
    #[allow(dead_code)]
    factory: IDXGIFactory5,
    #[allow(dead_code)]
    adapter: IDXGIAdapter1,
    device: ID3D11Device,
    #[allow(dead_code)]
    context: ID3D11DeviceContext,
}

fn create_d3d_device(adapter_luid: LUID) -> windows::core::Result<D3DDevice> {
    unsafe {
        let factory: IDXGIFactory5 = CreateDXGIFactory2(Default::default())?;
        let adapter: IDXGIAdapter1 = factory.EnumAdapterByLuid(adapter_luid)?;

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;

        Ok(D3DDevice {
            factory,
            adapter,
            device: device.expect("D3D11CreateDevice succeeded"),
            context: context.expect("D3D11CreateDevice succeeded"),
        })
    }
}

pub struct SwapChainProcessor {
    terminate_event: HANDLE,
    thread: Option<JoinHandle<()>>,
}

// The swap-chain handle and events are owned exclusively by the processor
// thread; the handle in this struct is only used to signal termination.
unsafe impl Send for SwapChainProcessor {}

impl SwapChainProcessor {
    pub fn start(
        swapchain: idd::IDDCX_SWAPCHAIN,
        render_adapter_luid: idd::LUID,
        new_frame_event: *mut c_void,
    ) -> Result<Self, ()> {
        let luid = LUID {
            LowPart: render_adapter_luid.LowPart,
            HighPart: render_adapter_luid.HighPart,
        };
        let device = create_d3d_device(luid).map_err(|_| ())?;

        let terminate_event = unsafe { CreateEventW(None, false, false, None) }.map_err(|_| ())?;

        // Raw pointers can't cross the thread boundary as-is; wrap them.
        struct ThreadArgs {
            swapchain: idd::IDDCX_SWAPCHAIN,
            new_frame_event: HANDLE,
            terminate_event: HANDLE,
            device: D3DDevice,
        }
        unsafe impl Send for ThreadArgs {}

        let args = ThreadArgs {
            swapchain,
            new_frame_event: HANDLE(new_frame_event),
            terminate_event,
            device,
        };

        // Confirm thread start before returning so termination signaling is
        // always paired with a live thread.
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("fluxidd-swapchain".into())
            .spawn(move || {
                let args = args;
                let _ = started_tx.send(());
                run_core(
                    args.swapchain,
                    &args.device,
                    args.new_frame_event,
                    args.terminate_event,
                );

                // Delete the swap-chain object to unblock IddCx teardown.
                unsafe {
                    wdk_sys::call_unsafe_wdf_function_binding!(
                        WdfObjectDelete,
                        args.swapchain as wdk_sys::WDFOBJECT,
                    );
                }
            })
            .map_err(|_| ())?;
        let _ = started_rx.recv();

        Ok(Self {
            terminate_event,
            thread: Some(thread),
        })
    }
}

impl Drop for SwapChainProcessor {
    fn drop(&mut self) {
        unsafe {
            let _ = SetEvent(self.terminate_event);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_core(
    swapchain: idd::IDDCX_SWAPCHAIN,
    device: &D3DDevice,
    new_frame_event: HANDLE,
    terminate_event: HANDLE,
) {
    let dxgi_device: IDXGIDevice = match device.device.cast() {
        Ok(d) => d,
        Err(_) => return,
    };

    let mut set_device = idd::IDARG_IN_SWAPCHAINSETDEVICE {
        pDevice: dxgi_device.as_raw() as *mut _,
    };
    let hr = unsafe { idd::IddCxSwapChainSetDevice(swapchain, &mut set_device) };
    if hr < 0 {
        return;
    }

    loop {
        let mut buffer = idd::IDARG_OUT_RELEASEANDACQUIREBUFFER::default();
        let hr = unsafe { idd::IddCxSwapChainReleaseAndAcquireBuffer(swapchain, &mut buffer) };

        if hr == E_PENDING {
            let handles = [new_frame_event, terminate_event];
            let wait = unsafe { WaitForMultipleObjects(&handles, false, 16) };
            if wait == WAIT_OBJECT_0 || wait == WAIT_TIMEOUT {
                continue;
            }
            // Terminate event or wait failure.
            break;
        } else if hr >= 0 {
            // Release the acquired surface immediately: presentation to a
            // physical display doesn't exist for a virtual monitor.
            unsafe {
                if !buffer.MetaData.pSurface.is_null() {
                    // The surface is returned with a reference we own.
                    let surface: windows::core::IUnknown =
                        core::mem::transmute(buffer.MetaData.pSurface);
                    drop(surface);
                }
            }

            let hr = unsafe { idd::IddCxSwapChainFinishedProcessingFrame(swapchain) };
            if hr < 0 {
                break;
            }
        } else {
            // Swap-chain likely abandoned (e.g. DXGI_ERROR_ACCESS_LOST).
            break;
        }
    }
}
