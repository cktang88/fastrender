//! `fastrender probe`, Metal 4 part: what of Metal 4 the device really has, one `metal4 <name>: <value>` line each
//! (CI greps and publishes them, .github/workflows/metal4.yml; docs/research/perf-audit-2026-11.md M-0 picks the
//! backend from them).
//!
//! Objects are only created when the device reports the Metal4 family, or with FASTRENDER_PROBE_MTL4_FORCE=1:
//! a device without the family may raise instead of returning nil, which aborts the process. Every stage prints
//! its line before the next one starts, so the output up to an abort still says how far Metal 4 got.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, NSObjectProtocol, ProtocolObject};
use objc2::{sel, ClassType};
use objc2_foundation::NSString;
use objc2_metal::*;
use objc2_metal_fx::*;

use crate::mac::Device;

pub fn probe(d: &Device) {
    let family = d.supportsFamily(MTLGPUFamily::Metal4);
    println!("metal4 supportsFamily(Metal4): {family}");
    let apple = [(MTLGPUFamily::Apple10, 10), (MTLGPUFamily::Apple9, 9), (MTLGPUFamily::Apple8, 8), (MTLGPUFamily::Apple7, 7)];
    let apple = apple.iter().find(|(f, _)| d.supportsFamily(*f)).map_or("<7".to_string(), |(_, n)| n.to_string());
    println!("metal4 apple family: {apple}, Metal3 family: {}", d.supportsFamily(MTLGPUFamily::Metal3));
    let (has_queue, has_compiler, has_residency) = (
        d.respondsToSelector(sel!(newMTL4CommandQueue)),
        d.respondsToSelector(sel!(newCompilerWithDescriptor:error:)),
        d.respondsToSelector(sel!(newResidencySetWithDescriptor:error:)),
    );
    println!(
        "metal4 selectors: newMTL4CommandQueue={has_queue} newCompilerWithDescriptor={has_compiler} newResidencySetWithDescriptor={has_residency}"
    );
    // class methods added in macOS 26: absent on older systems, so ask the class first
    let has = |c: &AnyClass| c.class_method(sel!(supportsMetal4FX:)).is_some();
    let fx = |name: &str, has: bool, f: &dyn Fn() -> bool| {
        println!("metal4 supportsMetal4FX({name}): {}", if has { f().to_string() } else { "no selector".into() });
    };
    fx("spatial", has(MTLFXSpatialScalerDescriptor::class()), &|| unsafe { MTLFXSpatialScalerDescriptor::supportsMetal4FX(d) });
    fx("temporal", has(MTLFXTemporalScalerDescriptor::class()), &|| unsafe { MTLFXTemporalScalerDescriptor::supportsMetal4FX(d) });
    match AnyClass::get(c"MTLFXFrameInterpolatorDescriptor") {
        Some(c) => fx("interpolator", has(c), &|| unsafe { MTLFXFrameInterpolatorDescriptor::supportsMetal4FX(d) }),
        None => println!("metal4 supportsMetal4FX(interpolator): no class"),
    }
    if !family && std::env::var_os("FASTRENDER_PROBE_MTL4_FORCE").is_none() {
        println!("metal4 objects: not attempted (no Metal4 family; FASTRENDER_PROBE_MTL4_FORCE=1 to try)");
        return;
    }
    let queue = if has_queue { d.newMTL4CommandQueue() } else { None };
    println!("metal4 newMTL4CommandQueue: {}", if !has_queue { "no selector" } else if queue.is_some() { "ok" } else { "nil" });
    let compiler = if has_compiler {
        match d.newCompilerWithDescriptor_error(&MTL4CompilerDescriptor::new()) {
            Ok(c) => {
                println!("metal4 newCompilerWithDescriptor: ok");
                Some(c)
            }
            Err(e) => {
                println!("metal4 newCompilerWithDescriptor: ERROR {}", e.localizedDescription());
                None
            }
        }
    } else {
        println!("metal4 newCompilerWithDescriptor: no selector");
        None
    };
    let residency = if has_residency {
        match d.newResidencySetWithDescriptor_error(&MTLResidencySetDescriptor::new()) {
            Ok(r) => {
                println!("metal4 newResidencySetWithDescriptor: ok");
                Some(r)
            }
            Err(e) => {
                println!("metal4 newResidencySetWithDescriptor: ERROR {}", e.localizedDescription());
                None
            }
        }
    } else {
        println!("metal4 newResidencySetWithDescriptor: no selector");
        None
    };
    match (queue, compiler, residency) {
        (Some(q), Some(c), Some(r)) => match frame(d, &q, &c, &r) {
            Ok(msg) => println!("metal4 frame: PASS {msg}"),
            Err(e) => println!("metal4 frame: FAIL {e}"),
        },
        _ => println!("metal4 frame: not attempted (needs the queue, the compiler and a residency set)"),
    }
}

const FRAME_MSL: &str = r#"
#include <metal_stdlib>
using namespace metal;
struct V { float4 pos [[position]]; };
vertex V probe_vs(uint vid [[vertex_id]]) {
    // a triangle over the middle of the target; the corners stay the clear colour
    float2 p[3] = { float2(-0.5, -0.5), float2(0.5, -0.5), float2(0.0, 0.5) };
    V o; o.pos = float4(p[vid], 0.0, 1.0); return o;
}
fragment float4 probe_fs() { return float4(0.0, 1.0, 0.0, 1.0); }
"#;

/// The smallest Metal 4 frame: MTL4Compiler library + pipeline, one render pass (clear red, a green triangle)
/// through MTL4CommandQueue / allocator / command buffer, then a readback of the centre and a corner.
fn frame(
    d: &Device,
    queue: &ProtocolObject<dyn MTL4CommandQueue>,
    compiler: &ProtocolObject<dyn MTL4Compiler>,
    residency: &ProtocolObject<dyn MTLResidencySet>,
) -> Result<String, String> {
    const N: usize = 64;
    let stage = |s: &str| println!("metal4 frame stage: {s}");
    let err = |e: Retained<objc2_foundation::NSError>| e.localizedDescription().to_string();

    stage("library (MTL4Compiler, MSL source)");
    let ld = MTL4LibraryDescriptor::new();
    ld.setSource(Some(&NSString::from_str(FRAME_MSL)));
    let lib = compiler.newLibraryWithDescriptor_error(&ld).map_err(|e| format!("library: {}", err(e)))?;

    stage("render pipeline (MTL4Compiler)");
    let func = |name: &str| {
        let f = MTL4LibraryFunctionDescriptor::new();
        f.setName(Some(&NSString::from_str(name)));
        f.setLibrary(Some(&lib));
        f
    };
    let pd = MTL4RenderPipelineDescriptor::new();
    pd.setVertexFunctionDescriptor(Some(&func("probe_vs")));
    pd.setFragmentFunctionDescriptor(Some(&func("probe_fs")));
    unsafe { pd.colorAttachments().objectAtIndexedSubscript(0) }.setPixelFormat(MTLPixelFormat::RGBA8Unorm);
    let pipeline = compiler
        .newRenderPipelineStateWithDescriptor_compilerTaskOptions_error(&pd, None)
        .map_err(|e| format!("pipeline: {}", err(e)))?;

    stage("target texture + residency set");
    let td = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(MTLPixelFormat::RGBA8Unorm, N, N, false)
    };
    td.setUsage(MTLTextureUsage::RenderTarget);
    td.setStorageMode(MTLStorageMode::Shared);
    let tex = d.newTextureWithDescriptor(&td).ok_or("texture: nil")?;
    residency.addAllocation(ProtocolObject::from_ref(&*tex));
    residency.commit();
    queue.addResidencySet(residency);

    stage("allocator + command buffer");
    let allocator = d.newCommandAllocator().ok_or("command allocator: nil")?;
    let cb = d.newCommandBuffer().ok_or("command buffer: nil")?;

    stage("encode (clear + triangle)");
    let rp = MTL4RenderPassDescriptor::new();
    let att = unsafe { rp.colorAttachments().objectAtIndexedSubscript(0) };
    att.setTexture(Some(&tex));
    att.setLoadAction(MTLLoadAction::Clear);
    att.setStoreAction(MTLStoreAction::Store);
    att.setClearColor(MTLClearColor { red: 1.0, green: 0.0, blue: 0.0, alpha: 1.0 });
    cb.beginCommandBufferWithAllocator(&allocator);
    let enc = cb.renderCommandEncoderWithDescriptor(&rp).ok_or("render encoder: nil")?;
    enc.setRenderPipelineState(&pipeline);
    unsafe { enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3) };
    enc.endEncoding();
    cb.endCommandBuffer();

    stage("commit + wait");
    let event = d.newSharedEvent().ok_or("shared event: nil")?;
    let mut list = [NonNull::from(&*cb)];
    unsafe { queue.commit_count(NonNull::new(list.as_mut_ptr()).unwrap(), 1) };
    queue.signalEvent_value(ProtocolObject::from_ref(&*event), 1);
    if !event.waitUntilSignaledValue_timeoutMS(1, 10_000) {
        return Err("the GPU did not finish within 10 s".into());
    }

    stage("readback");
    let mut px = vec![0u8; N * N * 4];
    let region = MTLRegion { origin: MTLOrigin { x: 0, y: 0, z: 0 }, size: MTLSize { width: N, height: N, depth: 1 } };
    unsafe {
        tex.getBytes_bytesPerRow_fromRegion_mipmapLevel(NonNull::new(px.as_mut_ptr() as *mut c_void).unwrap(), N * 4, region, 0)
    };
    let at = |x: usize, y: usize| <[u8; 4]>::try_from(&px[(y * N + x) * 4..][..4]).unwrap();
    let (centre, corner) = (at(N / 2, N / 2), at(1, 1));
    let msg = format!("centre rgba {centre:?} (want green), corner rgba {corner:?} (want red)");
    if centre == [0, 255, 0, 255] && corner == [255, 0, 0, 255] { Ok(msg) } else { Err(msg) }
}
