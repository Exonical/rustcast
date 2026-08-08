# FluxIdd — Flux virtual display driver (Rust, UMDF 2 / IddCx)

A pure-Rust Indirect Display Driver that adds a **virtual GPU-backed monitor** to
Windows, so a headless machine (no physical display) can be captured and streamed by
flux-server via the normal DXGI Desktop Duplication path at any resolution.

Built on [microsoft/windows-drivers-rs](https://github.com/microsoft/windows-drivers-rs)
(`wdk`/`wdk-sys`/`wdk-build`), with IddCx bindings generated from the WDK headers at
build time. Architecture mirrors Microsoft's IddCx sample driver.

## How it works

- The driver enumerates a virtual display adapter at device start.
- The monitor is **not** plugged in until flux-server sends `IOCTL_FLUXIDD_PLUG_IN`
  with a preferred `{width, height, refresh}` — the mode list then reports that mode
  first, plus a standard set (up to 3840x2160).
- `IOCTL_FLUXIDD_PLUG_OUT` removes the monitor again.
- The swap-chain processor thread acquires/releases the frames Windows renders for the
  monitor; actual capture + encode happens in flux-server through DXGI duplication of
  the new display.
- Device access is restricted to SYSTEM/Administrators (INF `Security` descriptor).

## Building (Windows only)

Prerequisites:

1. Visual Studio 2022 Build Tools with C++ workload
2. [WDK](https://learn.microsoft.com/windows-hardware/drivers/download-the-wdk) (or eWDK) — 22H2 or newer
3. LLVM/Clang 17 (for bindgen — newer LLVM (22+) miscompiles bindgen layouts,
   producing `E0080` size-assertion errors in wdk-sys):
   `winget install -i LLVM.LLVM --version 17.0.6`
   If a newer LLVM is also installed, point bindgen at 17 explicitly:
   `$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"` (the 17.0.6 install path)
4. `cargo install cargo-make --no-default-features --features tls-native`

Build & package:

```powershell
cd drivers\flux-idd
cargo make            # builds flux_idd.dll, stampinf + inf2cat + signs with a test cert
```

Output package (driver DLL, INF, catalog) lands in
`target\<profile>\flux-idd_package\`.

## Installing (test machine)

The driver is test-signed, so enable test signing once and reboot:

```powershell
bcdedit /set testsigning on
# reboot
```

Create the root-enumerated software device and install the driver
(using [devcon](https://learn.microsoft.com/windows-hardware/drivers/devtest/devcon) from the WDK, or `pnputil`):

```powershell
pnputil /add-driver FluxIdd.inf /install
devcon install FluxIdd.inf Root\FluxIdd
```

flux-server then opens the device via its interface GUID
`{5b1a4c37-6f5d-4a41-9c1d-8f2e4b6a7c01}` and sends plug-in/out IOCTLs.

For production distribution the package must be attestation-signed through the
[Windows Hardware Dev Center](https://learn.microsoft.com/windows-hardware/drivers/dashboard/).

## IOCTL interface

| IOCTL | Code | Input |
|---|---|---|
| `IOCTL_FLUXIDD_PLUG_IN` | `CTL_CODE(FILE_DEVICE_UNKNOWN, 0x900, METHOD_BUFFERED, FILE_WRITE_DATA)` | `{ width: u32, height: u32, refresh_hz: u32 }` (little-endian, packed) |
| `IOCTL_FLUXIDD_PLUG_OUT` | `CTL_CODE(FILE_DEVICE_UNKNOWN, 0x901, METHOD_BUFFERED, FILE_WRITE_DATA)` | none |

## Status

Untested scaffold: this crate has not yet been compiled against a real WDK (requires a
Windows box with WDK + LLVM). Expect iteration on the generated IddCx bindings —
especially bitfield accessors in `DISPLAYCONFIG_VIDEO_SIGNAL_INFO` and enum naming.
