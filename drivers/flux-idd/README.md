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
cargo make            # builds flux_idd.dll, stampinf + infverif + inf2cat + signs with a test cert
```

Run from a *Developer PowerShell for VS 2022* (or an environment where the WDK
tools `stampinf`/`infverif`/`inf2cat`/`signtool` are on PATH — they live in
`C:\Program Files (x86)\Windows Kits\10\bin\<sdk-version>\x64`).

Output package (driver DLL, stamped INF, signed catalog, test certificate)
lands in `target\debug\flux_idd_package\`.

## Installing (test machine)

The package is signed with an auto-generated test certificate
(`WDRLocalTestCert.cer`, included in the package folder), so a one-time setup
is needed on the test machine (elevated PowerShell):

```powershell
# Trust the test certificate
certutil -addstore Root WDRLocalTestCert.cer
certutil -addstore TrustedPublisher WDRLocalTestCert.cer

# Allow test-signed drivers to load (dev machines only), then reboot
bcdedit /set testsigning on
Restart-Computer
```

Then install the driver and create the root-enumerated software device
(from the `flux_idd_package` folder, elevated):

```powershell
pnputil /add-driver flux_idd.inf /install
# Create the device instance (devcon is in the WDK: ...\Windows Kits\10\Tools\<ver>\x64)
devcon install flux_idd.inf Root\FluxIdd
```

Verify: `pnputil /enum-devices /class Display` should list “Flux Virtual
Display” (started). No monitor appears yet — it plugs in on demand.

flux-server then opens the device via its interface GUID
`{5b1a4c37-6f5d-4a41-9c1d-8f2e4b6a7c01}` and sends plug-in/out IOCTLs.

For production distribution the package must be attestation-signed through the
[Windows Hardware Dev Center](https://learn.microsoft.com/windows-hardware/drivers/dashboard/).

## IOCTL interface

| IOCTL | Code | Input |
|---|---|---|
| `IOCTL_FLUXIDD_PLUG_IN` | `CTL_CODE(FILE_DEVICE_UNKNOWN, 0x900, METHOD_BUFFERED, FILE_WRITE_DATA)` | `{ width: u32, height: u32, refresh_hz: u32 }` (little-endian, packed) |
| `IOCTL_FLUXIDD_PLUG_OUT` | `CTL_CODE(FILE_DEVICE_UNKNOWN, 0x901, METHOD_BUFFERED, FILE_WRITE_DATA)` | none |
| `IOCTL_FLUXIDD_GET_STATUS` | `CTL_CODE(FILE_DEVICE_UNKNOWN, 0x902, METHOD_BUFFERED, FILE_READ_DATA)` | none; returns adapter initialization and monitor state |

## Diagnostics

Capture IddCx WPP tracing from an elevated command prompt:

```cmd
logman create trace IddCx -o IddCx.etl -ets -ow -mode sequential -p {D92BCB52-FA78-406F-A9A5-2037509FADEA} 0x4f4 0xFF
logman -stop IddCx -ets
```

For UMDF/WUDFHost failures, inspect Event Viewer at
`Applications and Services Logs → Microsoft → Windows →
DriverFrameworks-UserMode → Operational`, provider
`Microsoft-Windows-DriverFrameworks-UserMode`. Also inspect the
`Microsoft-Windows-IndirectDisplays-ClassExtension-Events` provider and the
System log around the failure timestamp.

## Status

Compiles and links against WDK 10.0.26100 (UMDF 2.31, IddCx 1.4). Runtime
behavior (monitor arrival, mode list, swap-chain processing) not yet verified
on a test machine.
