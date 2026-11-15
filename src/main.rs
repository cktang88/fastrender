#[cfg(target_os = "macos")]
fn main() {
    use objc2_foundation::NSString;
    use objc2_metal::*;
    let d = MTLCreateSystemDefaultDevice().expect("no Metal device");
    println!("device: {}", d.name());
    for (n, f) in [("Apple7", MTLGPUFamily::Apple7), ("Apple8", MTLGPUFamily::Apple8), ("Apple9", MTLGPUFamily::Apple9), ("Metal3", MTLGPUFamily::Metal3), ("Metal4", MTLGPUFamily::Metal4)] {
        println!("family {n}: {}", d.supportsFamily(f));
    }
    println!("unified memory: {}", d.hasUnifiedMemory());
    println!("working set MB: {}", d.recommendedMaxWorkingSetSize() / (1 << 20));
    unsafe {
        println!("metalfx spatial: {}", objc2_metal_fx::MTLFXSpatialScalerDescriptor::supportsDevice(&d));
        println!("metalfx temporal: {}", objc2_metal_fx::MTLFXTemporalScalerDescriptor::supportsDevice(&d));
    }
    let src = r#"
#include <metal_stdlib>
using namespace metal;
struct G { half4 lit [[color(0)]]; half4 alb [[color(1)]]; };
fragment G fbfetch(G in) { G o; o.lit = in.alb * 0.5h; o.alb = in.alb; return o; }
"#;
    let opts = MTLCompileOptions::new();
    match d.newLibraryWithSource_options_error(&NSString::from_str(src), Some(&opts)) {
        Ok(_) => println!("framebuffer-fetch shader compile: ok"),
        Err(e) => println!("framebuffer-fetch shader compile: ERR {}", e.localizedDescription()),
    }
}
#[cfg(not(target_os = "macos"))]
fn main() { eprintln!("fastrender runs on macOS only"); }
