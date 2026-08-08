//! Build script: standard WDK/UMDF configuration via wdk-build, plus custom
//! bindgen bindings for IddCx (not shipped by wdk-sys).

use std::env;
use std::path::PathBuf;

// IddCx exposes its API as FORCEINLINE functions dispatching through the
// IddCxFunctions table; `wrap_static_fns` makes bindgen emit callable extern
// wrappers for them (compiled below with cc).

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Configure UMDF compile/link flags from the [package.metadata.wdk] config.
    let config = wdk_build::Config::from_env_auto()?;
    config.configure_library_build()?;

    generate_iddcx_bindings(&config)?;

    // IddCx stub library for the IddCx* entry points.
    println!("cargo:rustc-link-lib=IddCxStub");
    println!("cargo:rustc-link-lib=onecoreuap");

    Ok(())
}

fn generate_iddcx_bindings(config: &wdk_build::Config) -> Result<(), Box<dyn std::error::Error>> {
    let out_path = PathBuf::from(env::var("OUT_DIR")?).join("iddcx_bindings.rs");

    let mut builder = bindgen::Builder::default()
        .header_contents(
            "iddcx_wrapper.h",
            r#"
#include <windows.h>
#include <wudfwdm.h>
#include <wdf.h>
#define IDDCX_VERSION_MAJOR 1
#define IDDCX_VERSION_MINOR 4
#define IDDCX_MINIMUM_VERSION_REQUIRED 4
#include <iddcx.h>
"#,
        )
        .allowlist_item("Idd.*")
        .allowlist_item("IDDCX_.*")
        .allowlist_item("IDARG_.*")
        .allowlist_item("EVT_IDD_.*")
        .allowlist_item("PFN_IDD.*")
        .allowlist_item("DISPLAYCONFIG_.*")
        .derive_default(true)
        .layout_tests(false)
        .wrap_static_fns(true)
        .wrap_static_fns_path(PathBuf::from(env::var("OUT_DIR")?).join("iddcx_wrappers"))
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

    // Compile the generated static-fn wrappers. In C++ mode bindgen writes
    // `iddcx_wrappers.cpp` but emits the wrapper functions without `extern
    // "C"`, while the Rust bindings expect unmangled names — so wrap the
    // wrapper-function section (everything after the "// Static wrappers"
    // marker, keeping the header includes outside) in an extern "C" block.
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let wrappers_generated = out_dir.join("iddcx_wrappers.cpp");
    let wrappers_cpp = out_dir.join("iddcx_wrappers_externc.cpp");
    let wrapper_src = std::fs::read_to_string(&wrappers_generated)?;
    const MARKER: &str = "// Static wrappers";
    let (headers, wrappers) = wrapper_src
        .split_once(MARKER)
        .ok_or("bindgen wrapper file missing '// Static wrappers' marker")?;
    std::fs::write(
        &wrappers_cpp,
        format!("{headers}\nextern \"C\" {{\n{MARKER}{wrappers}\n}}\n"),
    )?;

    let mut cc_build = cc::Build::new();
    cc_build.file(&wrappers_cpp);
    cc_build.cpp(true);
    cc_build.flag_if_supported("/std:c++17");
    cc_build.include(&iddcx_dir);
    cc_build.include(&out_dir);
    for include_dir in config.include_paths()? {
        cc_build.include(include_dir);
    }
    cc_build
        .define("IDDCX_VERSION_MAJOR", "1")
        .define("IDDCX_VERSION_MINOR", "4")
        .define("IDDCX_MINIMUM_VERSION_REQUIRED", "4")
        .compile("iddcx_wrappers");

    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}

/// IddCx.h is not on the standard WDK include paths: it lives in a versioned
/// subdirectory `Include\<sdk-version>\um\iddcx\<iddcx-version>\` (newer
/// WDKs) or directly in `um\iddcx\`. Probe the WDK include paths for it and
/// return the newest matching directory.
fn find_iddcx_include_dir(
    config: &wdk_build::Config,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    for include_dir in config.include_paths()? {
        let iddcx_root = include_dir.join("iddcx");
        if iddcx_root.join("IddCx.h").exists() {
            candidates.push(iddcx_root.clone());
        }
        if let Ok(entries) = std::fs::read_dir(&iddcx_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.join("IddCx.h").exists() {
                    candidates.push(path);
                }
            }
        }
    }

    // Version directories sort lexicographically well enough (e.g. 1.10 > 1.9
    // is the one wrinkle, so compare numerically when both parse).
    candidates.sort_by(|a, b| {
        let ver = |p: &PathBuf| -> Option<(u32, u32)> {
            let name = p.file_name()?.to_str()?;
            let (major, minor) = name.split_once('.')?;
            Some((major.parse().ok()?, minor.parse().ok()?))
        };
        match (ver(a), ver(b)) {
            (Some(va), Some(vb)) => va.cmp(&vb),
            _ => a.cmp(b),
        }
    });

    candidates.pop().ok_or_else(|| {
        "IddCx.h not found under any WDK include path (looked for um\\iddcx[\\<version>]\\IddCx.h). Is the full WDK installed?"
            .into()
    })
}
