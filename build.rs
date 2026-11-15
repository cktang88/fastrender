//! Built-in shaders, compiled offline (as Apple recommends): shaders/fastrender.metal -> fastrender.metallib with
//! `xcrun metal` + `xcrun metallib`, embedded by src/mac_programs.rs (`cfg(fr_metallib)`). When the target is
//! not macOS, or the build machine has no Metal toolchain (cross-checks on Linux, docs.rs, a Mac without Xcode),
//! nothing is embedded and the renderer compiles the same source at launch, as before.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(fr_metallib)");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=shaders/fastrender.metal");
    println!("cargo::rerun-if-env-changed=FASTRENDER_NO_METALLIB");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") || std::env::var_os("DOCS_RS").is_some() || std::env::var_os("FASTRENDER_NO_METALLIB").is_some() {
        return;
    }
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let (air, lib) = (out.join("fastrender.air"), out.join("fastrender.metallib"));
    let run = |args: &[&std::ffi::OsStr]| -> Result<(), String> {
        let o = Command::new("xcrun").args(args).output().map_err(|e| format!("xcrun: {e}"))?;
        if o.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&o.stderr).lines().take(6).collect::<Vec<_>>().join(" | ")) }
    };
    let s = std::ffi::OsStr::new;
    // -std=metal3.0 / macOS 14: the oldest the renderer runs on (a newer OS loads it as is); CI checks the same flags
    let built = run(&[s("-sdk"), s("macosx"), s("metal"), s("-std=metal3.0"), s("-mmacosx-version-min=14.0"), s("-c"), s("shaders/fastrender.metal"), s("-o"), air.as_os_str()])
        .and_then(|()| run(&[s("-sdk"), s("macosx"), s("metallib"), air.as_os_str(), s("-o"), lib.as_os_str()]));
    match built {
        Ok(()) => println!("cargo::rustc-cfg=fr_metallib"),
        Err(e) => println!("cargo::warning=built-in shaders not precompiled (the renderer compiles them at launch): {e}"),
    }
}
