//! Build script: standard WDK/UMDF configuration via wdk-build, plus custom
//! bindgen bindings for IddCx (not shipped by wdk-sys).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

// This ABI version is intentionally pinned; selecting the newest WDK
// directory would silently change bindgen layouts.
const IDDCX_VERSION_MAJOR: u32 = 1;
const IDDCX_VERSION_MINOR: u32 = 4;
const IDDCX_MINIMUM_VERSION_REQUIRED: u32 = 4;
const IDDCX_VERSION_DIRECTORY: &str = "1.4";

// IddCx exposes its API as FORCEINLINE functions dispatching through the
// IddCxFunctions table. Defining IDD_STUB before including iddcx.h makes the
// header declare the IddCx* entry points as plain imports instead, resolved
// by IddCxStub.lib — so bindgen emits directly callable extern functions.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Configure UMDF compile/link flags from the [package.metadata.wdk]
    // config. This is the leaf driver crate producing the driver DLL, so use
    // the binary configuration: it also emits the WDK library search paths
    // and UMDF link libraries (WdfDriverStubUm, which provides WdfFunctions/
    // WdfDriverGlobals) that a plain library build does not.
    let config = wdk_build::Config::from_env_auto()?;
    config.configure_binary_build()?;

    generate_iddcx_bindings(&config)?;

    // IddCx stub library for the IddCx* entry points.
    println!("cargo:rustc-link-lib=IddCxStub");
    println!("cargo:rustc-link-lib=onecoreuap");

    Ok(())
}

fn generate_iddcx_bindings(config: &wdk_build::Config) -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let out_path = out_dir.join("iddcx_bindings.rs");
    fs::write(
        out_dir.join("iddcx_version.rs"),
        format!(
            "pub const FLUX_IDDCX_VERSION_MAJOR: u32 = {IDDCX_VERSION_MAJOR};\n\
             pub const FLUX_IDDCX_VERSION_MINOR: u32 = {IDDCX_VERSION_MINOR};\n\
             pub const FLUX_IDDCX_MINIMUM_VERSION_REQUIRED: u32 = {IDDCX_MINIMUM_VERSION_REQUIRED};\n"
        ),
    )?;

    let mut builder = bindgen::Builder::default()
        .header_contents(
            "iddcx_wrapper.h",
            &format!(
                r#"
#include <windows.h>
#include <wudfwdm.h>
#include <wdf.h>
#define IDDCX_VERSION_MAJOR {IDDCX_VERSION_MAJOR}
#define IDDCX_VERSION_MINOR {IDDCX_VERSION_MINOR}
#define IDDCX_MINIMUM_VERSION_REQUIRED {IDDCX_MINIMUM_VERSION_REQUIRED}
#define IDD_STUB
#include <iddcx.h>
"#
            ),
        )
        .allowlist_item("Idd.*")
        .allowlist_item("IDDCX_.*")
        .allowlist_item("IDARG_.*")
        .allowlist_item("EVT_IDD_.*")
        .allowlist_item("PFN_IDD.*")
        .allowlist_item("IDDFUNC.*")
        // Declared extern in the headers but must be *defined* by the client
        // driver (see src/bindings.rs), which IddCxStub.lib links against.
        .blocklist_item("IddMinimumVersionRequired")
        .allowlist_item("DISPLAYCONFIG_.*")
        .derive_default(true)
        .layout_tests(false)
        // Generate bindings that are valid in this crate's Rust 2024 edition.
        // In particular, this enables `unsafe extern` blocks for imported
        // IddCx symbols without requiring a newer bindgen dependency.
        .rust_target(bindgen::RustTarget::Stable_1_82)
        .rust_edition(bindgen::RustEdition::Edition2024)
        // The IddCx headers are C++-only (forward struct references in
        // function signatures), so parse them as C++.
        .clang_arg("-x")
        .clang_arg("c++")
        .clang_arg("-std=c++17")
        // The WDF/IddCx headers rely on MSVC-specific behavior (enum forward
        // declarations with fixed underlying types, etc.).
        .clang_arg("-fms-compatibility")
        .clang_arg("-fms-extensions");

    let iddcx_dir = find_iddcx_include_dir(config)?;
    builder = builder.clang_arg(format!("-I{}", iddcx_dir.display()));
    for include_dir in config.include_paths()? {
        builder = builder.clang_arg(format!("-I{}", include_dir.display()));
    }

    builder.generate()?.write_to_file(&out_path)?;

    // IddCxStub.lib lives in the versioned lib directory mirroring the
    // include layout: Lib\<sdk>\um\<arch>\iddcx\<version>\.
    if let Some(lib_dir) = iddcx_stub_lib_dir(&iddcx_dir) {
        println!("cargo:rustc-link-search={}", lib_dir.display());
    }

    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

/// Map an IddCx include dir like `...\Include\<sdk>\um\iddcx\<ver>` to the
/// matching `...\Lib\<sdk>\um\<arch>\iddcx\<ver>` library directory.
fn iddcx_stub_lib_dir(iddcx_include_dir: &Path) -> Option<PathBuf> {
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").ok()?.as_str() {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => return None,
    };

    let version = iddcx_include_dir.file_name()?;
    let um_dir = iddcx_include_dir.parent()?.parent()?; // ...\Include\<sdk>\um
    let sdk_dir = um_dir.parent()?; // ...\Include\<sdk>
    let kits_root = sdk_dir.parent()?.parent()?; // ...\Windows Kits\10

    let lib_dir = kits_root
        .join("Lib")
        .join(sdk_dir.file_name()?)
        .join("um")
        .join(arch)
        .join("iddcx")
        .join(version);
    lib_dir.is_dir().then_some(lib_dir)
}

/// IddCx.h is not on the standard WDK include paths: it lives in the pinned
/// subdirectory `Include\<sdk-version>\um\iddcx\<iddcx-version>\`. Do not
/// fall back to another version because the generated layout is ABI-sensitive.
fn find_iddcx_include_dir(
    config: &wdk_build::Config,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    for include_dir in config.include_paths()? {
        let iddcx_root = include_dir.join("iddcx");
        let pinned_dir = iddcx_root.join(IDDCX_VERSION_DIRECTORY);
        if pinned_dir.join("IddCx.h").exists() {
            return Ok(pinned_dir);
        }
    }
    Err(format!(
        "pinned IddCx {IDDCX_VERSION_DIRECTORY} headers not found under any WDK include path; refusing to select another version"
    )
    .into())
}
