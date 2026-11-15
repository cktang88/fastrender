//! JavaScriptCore host (macOS's own JS engine: JIT, nothing to download). Runs `js/three-shim.js`
//! plus a game script, and hands the script a command buffer that is *Rust memory* exposed to JS as
//! an ArrayBuffer without copying (`JSObjectMakeArrayBufferWithBytesNoCopy`). Each frame we call the
//! script's global `frame(t, dt)` once, then read the buffer: one JS<->native crossing per frame.
//!
//! Minimal hand-written FFI to the stable JavaScriptCore C API (JavaScriptCore.framework).

#![allow(non_camel_case_types, unsafe_op_in_unsafe_fn)]

use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr::{null, null_mut};
use std::time::Instant;

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
    fn JSValueMakeFromJSONString(ctx: JSContextRef, s: JSStringRef) -> JSValueRef;
    fn JSValueToStringCopy(ctx: JSContextRef, v: JSValueRef, exc: *mut JSValueRef) -> JSStringRef;
    fn JSValueProtect(ctx: JSContextRef, v: JSValueRef);
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

unsafe extern "C" fn no_dealloc(_bytes: *mut c_void, _ctx: *mut c_void) {}

pub struct ScriptHost {
    ctx: JSGlobalContextRef,
    frame_fn: JSObjectRef,
    /// the command buffer JS writes into; must outlive `ctx` (released first in Drop)
    buf: Box<[u32]>,
    /// wall time of the last `frame()` call (JS work + command encoding)
    pub last_js_ms: f64,
}

impl ScriptHost {
    /// `params_json` becomes `__fr.params` (e.g. `{"boxes": 20000}`).
    pub fn new(shim: &str, game: &str, game_name: &str, params_json: &str, words: usize) -> Result<Self, String> {
        unsafe {
            let ctx = JSGlobalContextCreate(null_mut());
            let mut host = ScriptHost { ctx, frame_fn: null_mut(), buf: vec![0u32; words].into_boxed_slice(), last_js_ms: 0.0 };
            let global = JSContextGetGlobalObject(ctx);
            // __fr = { cmd: ArrayBuffer(over host.buf), log, params }
            let fr = JSObjectMake(ctx, null_mut(), null_mut());
            let mut exc: JSValueRef = null();
            let ab = JSObjectMakeArrayBufferWithBytesNoCopy(ctx, host.buf.as_mut_ptr() as *mut c_void, host.buf.len() * 4, no_dealloc, null_mut(), &mut exc);
            if ab.is_null() {
                return Err(format!("ArrayBuffer: {}", describe_exception(ctx, exc)));
            }
            JSObjectSetProperty(ctx, fr, JsStr::new("cmd").0, ab as JSValueRef, 0, null_mut());
            let log = JSObjectMakeFunctionWithCallback(ctx, JsStr::new("log").0, js_log);
            JSObjectSetProperty(ctx, fr, JsStr::new("log").0, log as JSValueRef, 0, null_mut());
            let params = JSValueMakeFromJSONString(ctx, JsStr::new(params_json).0);
            if !params.is_null() {
                JSObjectSetProperty(ctx, fr, JsStr::new("params").0, params, 0, null_mut());
            }
            JSObjectSetProperty(ctx, global, JsStr::new("__fr").0, fr as JSValueRef, 0, null_mut());
            // console.log for convenience
            let console = JSObjectMake(ctx, null_mut(), null_mut());
            JSObjectSetProperty(ctx, console, JsStr::new("log").0, log as JSValueRef, 0, null_mut());
            JSObjectSetProperty(ctx, global, JsStr::new("console").0, console as JSValueRef, 0, null_mut());
            host.eval(shim, "three-shim.js")?;
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

    /// Run one frame of the script; returns the command words it wrote.
    pub fn frame(&mut self, t: f64, dt: f64) -> Result<&[u32], String> {
        let start = Instant::now();
        unsafe {
            self.buf[0] = 0; // so a frame that forgets renderer.render() is caught by the decoder
            let args = [JSValueMakeNumber(self.ctx, t), JSValueMakeNumber(self.ctx, dt)];
            let mut exc: JSValueRef = null();
            JSObjectCallAsFunction(self.ctx, self.frame_fn, null_mut(), 2, args.as_ptr(), &mut exc);
            if !exc.is_null() {
                return Err(format!("frame(): {}", describe_exception(self.ctx, exc)));
            }
        }
        self.last_js_ms = start.elapsed().as_secs_f64() * 1000.0;
        Ok(&self.buf)
    }
}

impl Drop for ScriptHost {
    fn drop(&mut self) {
        unsafe {
            if !self.frame_fn.is_null() {
                JSValueUnprotect(self.ctx, self.frame_fn as JSValueRef);
            }
            JSGlobalContextRelease(self.ctx);
        }
    }
}
