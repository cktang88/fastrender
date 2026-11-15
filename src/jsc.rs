//! JavaScriptCore host (macOS's own JS engine: JIT, nothing to download). Runs `js/three-shim.js`
//! plus a game script, and hands the script a command buffer that is *Rust memory* exposed to JS as
//! an ArrayBuffer without copying (`JSObjectMakeArrayBufferWithBytesNoCopy`). Each frame we call the
//! script's global `frame(t, dt)` once, then read the buffer: one JS<->native crossing per frame.
//!
//! Minimal hand-written FFI to the stable JavaScriptCore C API (JavaScriptCore.framework).

#![allow(non_camel_case_types, unsafe_op_in_unsafe_fn)]

use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::image::NativeTime;

use crate::three::{Array, Resource};

type JSContextRef = *const c_void;
type JSGlobalContextRef = *mut c_void;
type JSStringRef = *mut c_void;
type JSValueRef = *const c_void;
type JSObjectRef = *mut c_void;
type JSClassRef = *mut c_void;
type CallAsFunction = unsafe extern "C" fn(JSContextRef, JSObjectRef, JSObjectRef, usize, *const JSValueRef, *mut JSValueRef) -> JSValueRef;
type BytesDeallocator = unsafe extern "C" fn(*mut c_void, *mut c_void);

#[link(name = "JavaScriptCore", kind = "framework")]
unsafe extern "C" {
    fn JSGlobalContextCreate(global_class: JSClassRef) -> JSGlobalContextRef;
    fn JSGlobalContextRelease(ctx: JSGlobalContextRef);
    fn JSContextGetGlobalObject(ctx: JSContextRef) -> JSObjectRef;
    fn JSStringCreateWithUTF8CString(s: *const c_char) -> JSStringRef;
    fn JSStringRelease(s: JSStringRef);
    fn JSStringGetMaximumUTF8CStringSize(s: JSStringRef) -> usize;
    fn JSStringGetUTF8CString(s: JSStringRef, buf: *mut c_char, size: usize) -> usize;
    fn JSEvaluateScript(ctx: JSContextRef, script: JSStringRef, this: JSObjectRef, url: JSStringRef, line: i32, exc: *mut JSValueRef) -> JSValueRef;
    fn JSObjectMake(ctx: JSContextRef, class: JSClassRef, data: *mut c_void) -> JSObjectRef;
    fn JSObjectSetProperty(ctx: JSContextRef, obj: JSObjectRef, name: JSStringRef, value: JSValueRef, attrs: u32, exc: *mut JSValueRef);
    fn JSObjectGetProperty(ctx: JSContextRef, obj: JSObjectRef, name: JSStringRef, exc: *mut JSValueRef) -> JSValueRef;
    fn JSObjectMakeFunctionWithCallback(ctx: JSContextRef, name: JSStringRef, f: CallAsFunction) -> JSObjectRef;
    fn JSObjectCallAsFunction(ctx: JSContextRef, f: JSObjectRef, this: JSObjectRef, argc: usize, argv: *const JSValueRef, exc: *mut JSValueRef) -> JSValueRef;
    fn JSObjectIsFunction(ctx: JSContextRef, obj: JSObjectRef) -> bool;
    fn JSObjectMakeArrayBufferWithBytesNoCopy(ctx: JSContextRef, bytes: *mut c_void, len: usize, dealloc: BytesDeallocator, dealloc_ctx: *mut c_void, exc: *mut JSValueRef) -> JSObjectRef;
    fn JSValueIsObject(ctx: JSContextRef, v: JSValueRef) -> bool;
    fn JSValueMakeNumber(ctx: JSContextRef, n: f64) -> JSValueRef;
    fn JSValueMakeUndefined(ctx: JSContextRef) -> JSValueRef;
    fn JSValueMakeNull(ctx: JSContextRef) -> JSValueRef;
    fn JSValueMakeString(ctx: JSContextRef, s: JSStringRef) -> JSValueRef;
    fn JSValueMakeFromJSONString(ctx: JSContextRef, s: JSStringRef) -> JSValueRef;
    fn JSValueToStringCopy(ctx: JSContextRef, v: JSValueRef, exc: *mut JSValueRef) -> JSStringRef;
    fn JSValueProtect(ctx: JSContextRef, v: JSValueRef);
    fn JSValueToNumber(ctx: JSContextRef, v: JSValueRef, exc: *mut JSValueRef) -> f64;
    fn JSValueGetTypedArrayType(ctx: JSContextRef, v: JSValueRef, exc: *mut JSValueRef) -> u32;
    fn JSObjectGetTypedArrayBytesPtr(ctx: JSContextRef, obj: JSObjectRef, exc: *mut JSValueRef) -> *mut c_void;
    fn JSObjectGetTypedArrayByteLength(ctx: JSContextRef, obj: JSObjectRef, exc: *mut JSValueRef) -> usize;
    fn JSValueUnprotect(ctx: JSContextRef, v: JSValueRef);
}

/// Owned JSStringRef from a Rust str.
struct JsStr(JSStringRef);
impl JsStr {
    fn new(s: &str) -> Self {
        let c = CString::new(s.replace('\0', "")).unwrap();
        JsStr(unsafe { JSStringCreateWithUTF8CString(c.as_ptr()) })
    }
}
impl Drop for JsStr {
    fn drop(&mut self) { unsafe { JSStringRelease(self.0) } }
}

unsafe fn to_rust_string(ctx: JSContextRef, v: JSValueRef) -> String {
    let s = JSValueToStringCopy(ctx, v, null_mut());
    if s.is_null() {
        return "<unprintable>".into();
    }
    let n = JSStringGetMaximumUTF8CStringSize(s);
    let mut buf = vec![0 as c_char; n.max(1)];
    JSStringGetUTF8CString(s, buf.as_mut_ptr(), n);
    JSStringRelease(s);
    CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
}

/// "Error: message" plus the JS stack when the exception carries one.
unsafe fn describe_exception(ctx: JSContextRef, exc: JSValueRef) -> String {
    let mut msg = to_rust_string(ctx, exc);
    if JSValueIsObject(ctx, exc) {
        let stack = JSObjectGetProperty(ctx, exc as JSObjectRef, JsStr::new("stack").0, null_mut());
        if !stack.is_null() {
            let s = to_rust_string(ctx, stack);
            if s != "undefined" && !s.is_empty() {
                msg.push_str("\n  at ");
                msg.push_str(&s.replace('\n', "\n  at "));
            }
        }
    }
    msg
}

unsafe extern "C" fn js_log(ctx: JSContextRef, _f: JSObjectRef, _this: JSObjectRef, argc: usize, argv: *const JSValueRef, _exc: *mut JSValueRef) -> JSValueRef {
    let parts: Vec<String> = (0..argc).map(|i| to_rust_string(ctx, *argv.add(i))).collect();
    println!("[js] {}", parts.join(" "));
    JSValueMakeUndefined(ctx)
}

/// `__fr.upload` calls / typed-array bytes copied out of JS by them (`fastrender jsbench`)
static UPLOADS: AtomicU64 = AtomicU64::new(0);
static UPLOAD_BYTES: AtomicU64 = AtomicU64::new(0);

/// __fr.now() -> wall-clock milliseconds since the first call (sub-microsecond; JavaScriptCore's own `Date.now()` is
/// whole milliseconds and the game's `performance.now` is the driver's virtual clock)
unsafe extern "C" fn js_now(ctx: JSContextRef, _f: JSObjectRef, _this: JSObjectRef, _argc: usize, _argv: *const JSValueRef, _exc: *mut JSValueRef) -> JSValueRef {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    JSValueMakeNumber(ctx, START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0)
}

unsafe extern "C" fn no_dealloc(_bytes: *mut c_void, _ctx: *mut c_void) {}

thread_local! {
    /// resource uploads/releases from `__fr._upload` / `__fr._release`, drained by the renderer
    static RESOURCES: std::cell::RefCell<Vec<Resource>> = const { std::cell::RefCell::new(Vec::new()) };
    /// programs translated for `__fr.program` (protocol v3), drained by the renderer: (program id, three's cache key, program)
    static PROGRAMS: std::cell::RefCell<Vec<(u32, String, crate::xlat::Program)>> = const { std::cell::RefCell::new(Vec::new()) };
    /// the shader cache `__fr.program` translates through
    static XLAT_CACHE: crate::xlat::Cache = crate::xlat::Cache::open_default();
}

/// __fr.program(key, vertexGlsl, fragmentGlsl, id) -> layout JSON string, or null when translation failed (logged).
/// `id` is the short id material uploads refer to the program by.
/// Synchronous: three's program is needed for this frame's uniform packing. The translation itself is the
/// host-independent `xlat::host_program` (the Electron addon calls the same function).
unsafe extern "C" fn js_program(ctx: JSContextRef, _f: JSObjectRef, _this: JSObjectRef, argc: usize, argv: *const JSValueRef, _exc: *mut JSValueRef) -> JSValueRef {
    if argc < 3 {
        return JSValueMakeNull(ctx);
    }
    let _t = NativeTime::new();
    let key = to_rust_string(ctx, *argv);
    let vs = to_rust_string(ctx, *argv.add(1));
    let fs = to_rust_string(ctx, *argv.add(2));
    let id = if argc > 3 { JSValueToNumber(ctx, *argv.add(3), null_mut()) as u32 } else { 0 };
    let t = Instant::now();
    match XLAT_CACHE.with(|c| crate::xlat::host_program(c, &key, &vs, &fs)) {
        Ok((json, program)) => {
            if std::env::var_os("FASTRENDER_LOG_PROGRAMS").is_some() {
                println!("program {} translated in {:.1} ms", key.split(',').next().unwrap_or(""), t.elapsed().as_secs_f64() * 1000.0);
            }
            PROGRAMS.with(|p| p.borrow_mut().push((id, key, program)));
            JSValueMakeString(ctx, JsStr::new(&json).0)
        }
        Err(e) => {
            eprintln!("host.program: {e}");
            JSValueMakeNull(ctx)
        }
    }
}


// JSTypedArrayType values (JavaScriptCore/JSValueRef.h)
const TA_UINT8: u32 = 3;
const TA_UINT8_CLAMPED: u32 = 4;
const TA_UINT16: u32 = 5;
const TA_UINT32: u32 = 6;
const TA_FLOAT32: u32 = 7;

/// copy a typed array argument out of JS memory (None for null/undefined/non-typed-arrays)
unsafe fn typed_array(ctx: JSContextRef, v: JSValueRef) -> Option<Array> {
    if v.is_null() || !JSValueIsObject(ctx, v) {
        return None;
    }
    let ty = JSValueGetTypedArrayType(ctx, v, null_mut());
    let obj = v as JSObjectRef;
    let ptr = JSObjectGetTypedArrayBytesPtr(ctx, obj, null_mut()); // points at the view's first element
    let len = JSObjectGetTypedArrayByteLength(ctx, obj, null_mut());
    if ptr.is_null() && len > 0 {
        return None;
    }
    UPLOAD_BYTES.fetch_add(len as u64, Ordering::Relaxed);
    macro_rules! copy {
        ($t:ty, $variant:ident) => {
            Some(Array::$variant(std::slice::from_raw_parts(ptr as *const $t, len / std::mem::size_of::<$t>()).to_vec()))
        };
    }
    match ty {
        TA_FLOAT32 => copy!(f32, F32),
        TA_UINT16 => copy!(u16, U16),
        TA_UINT32 => copy!(u32, U32),
        TA_UINT8 | TA_UINT8_CLAMPED => copy!(u8, U8),
        _ => None,
    }
}

/// __fr._upload(kind, id, metaJson, ...typedArrays)
unsafe extern "C" fn js_upload(ctx: JSContextRef, _f: JSObjectRef, _this: JSObjectRef, argc: usize, argv: *const JSValueRef, _exc: *mut JSValueRef) -> JSValueRef {
    if argc >= 3 {
        let _t = NativeTime::new();
        UPLOADS.fetch_add(1, Ordering::Relaxed);
        let kind = JSValueToNumber(ctx, *argv, null_mut()) as u32;
        let id = JSValueToNumber(ctx, *argv.add(1), null_mut()) as u32;
        let meta = to_rust_string(ctx, *argv.add(2));
        let arrays = (3..argc).map(|i| typed_array(ctx, *argv.add(i))).collect();
        RESOURCES.with(|r| r.borrow_mut().push(Resource::Upload { kind, id, meta, arrays }));
    }
    JSValueMakeUndefined(ctx)
}

/// __fr._release(kind, id)
unsafe extern "C" fn js_release(ctx: JSContextRef, _f: JSObjectRef, _this: JSObjectRef, argc: usize, argv: *const JSValueRef, _exc: *mut JSValueRef) -> JSValueRef {
    if argc >= 2 {
        let kind = JSValueToNumber(ctx, *argv, null_mut()) as u32;
        let id = JSValueToNumber(ctx, *argv.add(1), null_mut()) as u32;
        RESOURCES.with(|r| r.borrow_mut().push(Resource::Release { kind, id }));
    }
    JSValueMakeUndefined(ctx)
}

/// JS side of the resource channel (protocol v2): meta goes over as JSON
const PRELUDE: &str = "__fr.upload = function (k, id, meta) { var a = [k, id, JSON.stringify(meta)]; for (var i = 3; i < arguments.length; i++) a.push(arguments[i]); return __fr._upload.apply(null, a); };\n__fr.release = function (k, id) { __fr._release(k, id); };\nif (typeof globalThis.performance === 'undefined') globalThis.performance = { now: function () { return Date.now(); } };";

pub struct ScriptHost {
    ctx: JSGlobalContextRef,
    frame_fn: JSObjectRef,
    /// the command buffer JS writes into (raw allocation: JS writes it, Rust only reads it through
    /// raw pointers, never through a &mut borrow); freed in Drop after the context is released
    buf: *mut u32,
    words: usize,
    /// wall time of the last `frame()` call (JS work + command encoding)
    pub last_js_ms: f64,
}

impl ScriptHost {
    /// `params_json` becomes `__fr.params` (e.g. `{"boxes": 20000}`).
    pub fn new(shim: &str, game: &str, game_name: &str, params_json: &str, words: usize) -> Result<Self, String> {
        unsafe {
            let ctx = JSGlobalContextCreate(null_mut());
            let buf = Box::into_raw(vec![0u32; words].into_boxed_slice()) as *mut u32;
            let mut host = ScriptHost { ctx, frame_fn: null_mut(), buf, words, last_js_ms: 0.0 };
            let global = JSContextGetGlobalObject(ctx);
            // __fr = { cmd: ArrayBuffer(over host.buf), log, params }
            let fr = JSObjectMake(ctx, null_mut(), null_mut());
            let mut exc: JSValueRef = null();
            let ab = JSObjectMakeArrayBufferWithBytesNoCopy(ctx, host.buf as *mut c_void, host.words * 4, no_dealloc, null_mut(), &mut exc);
            if ab.is_null() {
                return Err(format!("ArrayBuffer: {}", describe_exception(ctx, exc)));
            }
            JSObjectSetProperty(ctx, fr, JsStr::new("cmd").0, ab as JSValueRef, 0, null_mut());
            let log = JSObjectMakeFunctionWithCallback(ctx, JsStr::new("log").0, js_log);
            JSObjectSetProperty(ctx, fr, JsStr::new("log").0, log as JSValueRef, 0, null_mut());
            let up = JSObjectMakeFunctionWithCallback(ctx, JsStr::new("_upload").0, js_upload);
            JSObjectSetProperty(ctx, fr, JsStr::new("_upload").0, up as JSValueRef, 0, null_mut());
            let rel = JSObjectMakeFunctionWithCallback(ctx, JsStr::new("_release").0, js_release);
            JSObjectSetProperty(ctx, fr, JsStr::new("_release").0, rel as JSValueRef, 0, null_mut());
            let now = JSObjectMakeFunctionWithCallback(ctx, JsStr::new("now").0, js_now);
            JSObjectSetProperty(ctx, fr, JsStr::new("now").0, now as JSValueRef, 0, null_mut());
            let prog = JSObjectMakeFunctionWithCallback(ctx, JsStr::new("program").0, js_program);
            JSObjectSetProperty(ctx, fr, JsStr::new("program").0, prog as JSValueRef, 0, null_mut());
            crate::image::install(ctx as *const c_void, fr); // __fr.readFile / __fr.decodeImage (src/image.rs)
            let params = JSValueMakeFromJSONString(ctx, JsStr::new(params_json).0);
            if !params.is_null() {
                JSObjectSetProperty(ctx, fr, JsStr::new("params").0, params, 0, null_mut());
            }
            JSObjectSetProperty(ctx, global, JsStr::new("__fr").0, fr as JSValueRef, 0, null_mut());
            // console.log for convenience
            let console = JSObjectMake(ctx, null_mut(), null_mut());
            JSObjectSetProperty(ctx, console, JsStr::new("log").0, log as JSValueRef, 0, null_mut());
            JSObjectSetProperty(ctx, global, JsStr::new("console").0, console as JSValueRef, 0, null_mut());
            host.eval(PRELUDE, "prelude.js")?;
            if !shim.is_empty() {
                host.eval(shim, "three-shim.js")?;
            }
            host.eval(game, game_name)?;
            let f = JSObjectGetProperty(ctx, global, JsStr::new("frame").0, null_mut());
            if f.is_null() || !JSValueIsObject(ctx, f) || !JSObjectIsFunction(ctx, f as JSObjectRef) {
                return Err(format!("{game_name} must define a global function frame(t, dt)"));
            }
            JSValueProtect(ctx, f);
            host.frame_fn = f as JSObjectRef;
            Ok(host)
        }
    }

    fn eval(&self, src: &str, name: &str) -> Result<(), String> {
        unsafe {
            let mut exc: JSValueRef = null();
            JSEvaluateScript(self.ctx, JsStr::new(src).0, null_mut(), JsStr::new(name).0, 1, &mut exc);
            if !exc.is_null() {
                return Err(format!("{name}: {}", describe_exception(self.ctx, exc)));
            }
        }
        Ok(())
    }

    /// Evaluate `src` and return its result as a string (diagnostics: `fastrender jsbench`).
    pub fn eval_string(&self, src: &str) -> Result<String, String> {
        unsafe {
            let mut exc: JSValueRef = null();
            let v = JSEvaluateScript(self.ctx, JsStr::new(src).0, null_mut(), JsStr::new("eval.js").0, 1, &mut exc);
            if !exc.is_null() || v.is_null() {
                return Err(describe_exception(self.ctx, exc));
            }
            Ok(to_rust_string(self.ctx, v))
        }
    }

    /// Resource uploads/releases the script made since the last call (protocol v2).
    pub fn take_resources(&self) -> Vec<Resource> { RESOURCES.with(|r| std::mem::take(&mut *r.borrow_mut())) }

    /// Programs `__fr.program` translated since the last call (protocol v3), in call order.
    pub fn take_programs(&self) -> Vec<(u32, String, crate::xlat::Program)> { PROGRAMS.with(|p| std::mem::take(&mut *p.borrow_mut())) }

    /// Pay glslang's one-time process setup (~120 ms) on a background thread, off the first program's path.
    pub fn warm_up_translator() {
        let _ = std::thread::Builder::new().name("xlat-warm-up".into()).spawn(crate::xlat::warm_up);
    }

    /// Run one frame of the script; returns the command words it wrote.
    pub fn frame(&mut self, t: f64, dt: f64) -> Result<&[u32], String> {
        let start = Instant::now();
        unsafe {
            self.buf.write_volatile(0); // so a frame that forgets renderer.render() is caught by the decoder
            let args = [JSValueMakeNumber(self.ctx, t), JSValueMakeNumber(self.ctx, dt)];
            let mut exc: JSValueRef = null();
            JSObjectCallAsFunction(self.ctx, self.frame_fn, null_mut(), 2, args.as_ptr(), &mut exc);
            if !exc.is_null() {
                return Err(format!("frame(): {}", describe_exception(self.ctx, exc)));
            }
        }
        self.last_js_ms = start.elapsed().as_secs_f64() * 1000.0;
        // SAFETY: JS is not running (single-threaded host); the slice lives until the next frame()
        Ok(unsafe { std::slice::from_raw_parts(self.buf, self.words) })
    }
}

impl Drop for ScriptHost {
    fn drop(&mut self) {
        unsafe {
            if !self.frame_fn.is_null() {
                JSValueUnprotect(self.ctx, self.frame_fn as JSValueRef);
            }
            JSGlobalContextRelease(self.ctx);
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(self.buf, self.words)));
        }
    }
}

/// The in-process host: `frame()` runs the game's JS (src/host.rs).
impl crate::host::FrameSource for ScriptHost {
    fn frame(&mut self, t: f64, dt: f64) -> Result<&[u32], String> { ScriptHost::frame(self, t, dt) }
    fn take_programs(&mut self) -> Vec<(u32, String, crate::xlat::Program)> { ScriptHost::take_programs(self) }
    fn take_resources(&mut self) -> Vec<Resource> { ScriptHost::take_resources(self) }
    fn js_ms(&self) -> f64 { self.last_js_ms }
}

/// What the engine is doing, measured from inside it: a hot integer loop (a JIT runs it in a few ns per iteration,
/// the interpreter in ~10x that) and, where `$vm` exists (JSC_useDollarVM=1), which tiers ran a hot function.
const PROBE: &str = "(function () { var r = {}, now = __fr.now; function hot(n) { var s = 0; for (var i = 0; i < n; i++) s = (s + i * i) % 1000003; return s; } hot(1e5); var t = now(); hot(2e7); r.loop_ns_per_iter = +((now() - t) * 1e6 / 2e7).toFixed(2); var a = new Float32Array(4096); function fill(k) { for (var i = 0; i < 4096; i++) a[i] = i * k; return a[7]; } for (var j = 0; j < 200; j++) fill(j); t = now(); for (j = 0; j < 4000; j++) fill(j); r.f32_write_ns = +((now() - t) * 1e6 / (4000 * 4096)).toFixed(2); if (typeof $vm !== 'undefined') { var f = function () { return ($vm.ftlTrue() ? 4 : 0) | ($vm.dfgTrue() ? 2 : 0); }; var m = 0; for (var i = 0; i < 3e6; i++) m |= f(); r.tiers = (m & 4 ? 'FTL ' : '') + (m & 2 ? 'DFG' : '') || 'none above baseline'; } r.wasm = typeof WebAssembly; return JSON.stringify(r); })()";

/// `fastrender jsbench <bundle.js> [--warmup N] [--frames N] [--params JSON] [--window N]`: the game's JS alone in the
/// JavaScriptCore host (no Metal, no window): `frame()` per frame exactly as the renderer calls it, uploads and
/// programs drained and dropped. Prints JS ms/frame per window of frames, the time inside native `__fr.*` callbacks,
/// and the driver's own breakdown (`__worldCostReport`, js/examples/world-objects.drive.mjs). For A/B runs of
/// JavaScriptCore options (`JSC_useJIT=0`, ...), entitlements and signing: docs/research/jsc-vs-v8.md.
pub fn js_bench(args: &[String]) -> Result<(), String> {
    let bundle = args.first().ok_or("usage: fastrender jsbench <bundle.js> [--warmup N] [--frames N] [--params JSON] [--window N]")?;
    let opt = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let num = |k: &str, d: usize| opt(k).and_then(|v| v.parse().ok()).unwrap_or(d);
    let (warmup, frames, window) = (num("--warmup", 71), num("--frames", 30), num("--window", 10).max(1));
    let params = opt("--params").unwrap_or_else(|| "{\"boxes\": 0, \"lights\": 0, \"programs\": \"three\", \"width\": 1280, \"height\": 800}".into());
    let game = std::fs::read_to_string(bundle).map_err(|e| format!("bundle {bundle}: {e}"))?;
    let jsc_env: Vec<String> = std::env::vars().filter(|(k, _)| k.starts_with("JSC_")).map(|(k, v)| format!("{k}={v}")).collect();
    println!("jsbench: {bundle}, warmup {warmup}, frames {frames}, env [{}]", jsc_env.join(" "));
    ScriptHost::warm_up_translator();
    let t = Instant::now();
    let mut host = ScriptHost::new("", &game, bundle, &params, 4 << 20)?;
    println!("jsbench: bundle evaluated in {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);
    println!("jsbench probe: {}", host.eval_string(PROBE).unwrap_or_else(|e| format!("failed: {e}")));
    let counters = || {
        use crate::image::{DECODES, DECODE_NS, NATIVE_NS, READS};
        [NATIVE_NS.load(Ordering::Relaxed), UPLOADS.load(Ordering::Relaxed), UPLOAD_BYTES.load(Ordering::Relaxed), READS.load(Ordering::Relaxed), DECODES.load(Ordering::Relaxed), DECODE_NS.load(Ordering::Relaxed)]
    };
    let (mut js, mut native) = (Vec::with_capacity(frames), 0.0);
    let (mut w_js, mut w_n, mut w0) = (0.0, 0usize, counters());
    for i in 0..warmup + frames {
        if i == warmup {
            let _ = host.eval_string("globalThis.__worldCostReset ? (__worldCostReset(), 1) : 0");
        }
        let c0 = counters();
        host.frame(i as f64 / 60.0, 1.0 / 60.0)?;
        let (ms, c1) = (host.last_js_ms, counters());
        drop(host.take_resources());
        drop(host.take_programs());
        if i >= warmup {
            js.push(ms);
            native += (c1[0] - c0[0]) as f64 / 1e6;
        }
        w_js += ms;
        w_n += 1;
        if w_n == window || i + 1 == warmup + frames {
            let d: Vec<u64> = c1.iter().zip(w0).map(|(a, b)| a - b).collect();
            let n = w_n as f64;
            println!(
                "jsbench frames {}-{}: JS {:.1} ms/frame, of which native callbacks {:.1} ms (image decode {:.1}); per frame: {:.1} uploads, {:.2} MB, {:.1} reads, {:.1} decodes",
                i + 1 - w_n,
                i,
                w_js / n,
                d[0] as f64 / 1e6 / n,
                d[5] as f64 / 1e6 / n,
                d[1] as f64 / n,
                d[2] as f64 / 1e6 / n,
                d[3] as f64 / n,
                d[4] as f64 / n
            );
            (w_js, w_n, w0) = (0.0, 0, c1);
        }
    }
    if !js.is_empty() {
        let n = js.len() as f64;
        let mean = js.iter().sum::<f64>() / n;
        let mut s = js.clone();
        s.sort_by(|a, b| a.total_cmp(b));
        println!(
            "jsbench result: JS {:.2} ms/frame mean, {:.2} median, {:.2} min, {:.2} max over frames {}-{}; native callbacks {:.2} ms/frame; in-JS breakdown (ms/frame): {}",
            mean,
            s[s.len() / 2],
            s[0],
            s[s.len() - 1],
            warmup,
            warmup + frames - 1,
            native / n,
            host.eval_string("JSON.stringify(globalThis.__worldCostReport ? __worldCostReport() : null)").unwrap_or_default()
        );
    }
    Ok(())
}
