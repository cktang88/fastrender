//! Asset access for real web games in the JavaScriptCore host, which has no `fetch` and no image decoder:
//!
//! - `__fr.readFile(path)` -> ArrayBuffer | null: a file under `$FASTRENDER_ASSETS` (the game's static root, e.g.
//!   its `public/` dir); the JS side builds `fetch()` / `<img src>` on it (js/examples/world-objects.dom.mjs).
//! - `__fr.decodeImage(Uint8Array)` -> ArrayBuffer | null: PNG / JPEG / WebP decoded to RGBA8 by the `image` crate,
//!   laid out as `[width u32 LE][height u32 LE][rgba ...]`.
//!
//! Installed on `__fr` by one call from `jsc.rs`. The decoding and path rules are plain Rust (tested on any OS);
//! only the JavaScriptCore binding is macOS-only.

use std::path::{Component, Path, PathBuf};

/// RGBA8 pixels of an encoded image (PNG, JPEG, WebP), or None.
pub fn decode_rgba(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let img = ::image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    Some((w, h, img.into_raw()))
}

/// `[w][h][rgba]` as one buffer (what `__fr.decodeImage` returns)
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn decode_packed(bytes: &[u8]) -> Option<Vec<u8>> {
    let (w, h, px) = decode_rgba(bytes)?;
    let mut out = Vec::with_capacity(8 + px.len());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&px);
    Some(out)
}

/// `rel` under `root`, refusing absolute paths and `..` (the game only names files inside its static root)
pub fn asset_path(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = rel.trim_start_matches('/');
    let p = Path::new(rel);
    if rel.is_empty() || p.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(root.join(p))
}

/// a file of the game's static root (`$FASTRENDER_ASSETS`)
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn read_asset(rel: &str) -> Option<Vec<u8>> {
    let root = std::env::var_os("FASTRENDER_ASSETS")?;
    std::fs::read(asset_path(Path::new(&root), rel)?).ok()
}

#[cfg(target_os = "macos")]
mod jsc {
    #![allow(non_camel_case_types, unsafe_op_in_unsafe_fn)]
    use std::ffi::{c_char, c_void, CStr, CString};
    use std::ptr::null_mut;

    type JSContextRef = *const c_void;
    type JSStringRef = *mut c_void;
    type JSValueRef = *const c_void;
    type JSObjectRef = *mut c_void;
    type CallAsFunction = unsafe extern "C" fn(JSContextRef, JSObjectRef, JSObjectRef, usize, *const JSValueRef, *mut JSValueRef) -> JSValueRef;
    type BytesDeallocator = unsafe extern "C" fn(*mut c_void, *mut c_void);

    #[link(name = "JavaScriptCore", kind = "framework")]
    unsafe extern "C" {
        fn JSStringCreateWithUTF8CString(s: *const c_char) -> JSStringRef;
        fn JSStringRelease(s: JSStringRef);
        fn JSStringGetMaximumUTF8CStringSize(s: JSStringRef) -> usize;
        fn JSStringGetUTF8CString(s: JSStringRef, buf: *mut c_char, size: usize) -> usize;
        fn JSObjectSetProperty(ctx: JSContextRef, obj: JSObjectRef, name: JSStringRef, value: JSValueRef, attrs: u32, exc: *mut JSValueRef);
        fn JSObjectMakeFunctionWithCallback(ctx: JSContextRef, name: JSStringRef, f: CallAsFunction) -> JSObjectRef;
        fn JSObjectMakeArrayBufferWithBytesNoCopy(ctx: JSContextRef, bytes: *mut c_void, len: usize, dealloc: BytesDeallocator, dealloc_ctx: *mut c_void, exc: *mut JSValueRef) -> JSObjectRef;
        fn JSValueMakeNull(ctx: JSContextRef) -> JSValueRef;
        fn JSValueToStringCopy(ctx: JSContextRef, v: JSValueRef, exc: *mut JSValueRef) -> JSStringRef;
        fn JSValueGetTypedArrayType(ctx: JSContextRef, v: JSValueRef, exc: *mut JSValueRef) -> u32;
        fn JSObjectGetTypedArrayBytesPtr(ctx: JSContextRef, obj: JSObjectRef, exc: *mut JSValueRef) -> *mut c_void;
        fn JSObjectGetTypedArrayByteLength(ctx: JSContextRef, obj: JSObjectRef, exc: *mut JSValueRef) -> usize;
    }
    // JSTypedArrayType: kJSTypedArrayTypeUint8Array = 3, kJSTypedArrayTypeUint8ClampedArray = 4
    const U8: u32 = 3;
    const U8C: u32 = 4;

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

    unsafe fn string_arg(ctx: JSContextRef, v: JSValueRef) -> Option<String> {
        let s = JSValueToStringCopy(ctx, v, null_mut());
        if s.is_null() {
            return None;
        }
        let n = JSStringGetMaximumUTF8CStringSize(s);
        let mut buf = vec![0 as c_char; n.max(1)];
        JSStringGetUTF8CString(s, buf.as_mut_ptr(), n);
        JSStringRelease(s);
        Some(CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }

    unsafe extern "C" fn free_bytes(bytes: *mut c_void, len: *mut c_void) {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(bytes as *mut u8, len as usize)));
    }

    /// an ArrayBuffer that owns `v` (freed by JavaScriptCore's collector)
    unsafe fn array_buffer(ctx: JSContextRef, v: Vec<u8>) -> JSValueRef {
        let b = v.into_boxed_slice();
        let len = b.len();
        let ptr = Box::into_raw(b) as *mut u8;
        let ab = JSObjectMakeArrayBufferWithBytesNoCopy(ctx, ptr as *mut c_void, len, free_bytes, len as *mut c_void, null_mut());
        if ab.is_null() {
            free_bytes(ptr as *mut c_void, len as *mut c_void);
            return JSValueMakeNull(ctx);
        }
        ab as JSValueRef
    }

    unsafe extern "C" fn js_read_file(ctx: JSContextRef, _f: JSObjectRef, _this: JSObjectRef, argc: usize, argv: *const JSValueRef, _exc: *mut JSValueRef) -> JSValueRef {
        if argc == 0 {
            return JSValueMakeNull(ctx);
        }
        let path = string_arg(ctx, *argv);
        match path.and_then(|p| super::read_asset(&p)) {
            Some(bytes) => array_buffer(ctx, bytes),
            None => JSValueMakeNull(ctx),
        }
    }

    unsafe extern "C" fn js_decode_image(ctx: JSContextRef, _f: JSObjectRef, _this: JSObjectRef, argc: usize, argv: *const JSValueRef, _exc: *mut JSValueRef) -> JSValueRef {
        if argc == 0 {
            return JSValueMakeNull(ctx);
        }
        let v = *argv;
        let ty = JSValueGetTypedArrayType(ctx, v, null_mut());
        if ty != U8 && ty != U8C {
            return JSValueMakeNull(ctx);
        }
        let ptr = JSObjectGetTypedArrayBytesPtr(ctx, v as JSObjectRef, null_mut()) as *const u8;
        let len = JSObjectGetTypedArrayByteLength(ctx, v as JSObjectRef, null_mut());
        if ptr.is_null() || len == 0 {
            return JSValueMakeNull(ctx);
        }
        let bytes = std::slice::from_raw_parts(ptr, len);
        match super::decode_packed(bytes) {
            Some(out) => array_buffer(ctx, out),
            None => JSValueMakeNull(ctx),
        }
    }

    /// `__fr.readFile` + `__fr.decodeImage`
    pub unsafe fn install(ctx: *const c_void, fr: *mut c_void) {
        for (name, f) in [("readFile", js_read_file as CallAsFunction), ("decodeImage", js_decode_image as CallAsFunction)] {
            let n = JsStr::new(name);
            let func = JSObjectMakeFunctionWithCallback(ctx, n.0, f);
            JSObjectSetProperty(ctx, fr, n.0, func as JSValueRef, 0, null_mut());
        }
    }
}
#[cfg(target_os = "macos")]
pub use jsc::install;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_png_to_rgba() {
        let mut enc = Vec::new();
        {
            let mut e = png::Encoder::new(&mut enc, 2, 1);
            e.set_color(png::ColorType::Rgb);
            e.set_depth(png::BitDepth::Eight);
            e.write_header().unwrap().write_image_data(&[255, 0, 0, 0, 0, 255]).unwrap();
        }
        let (w, h, px) = decode_rgba(&enc).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(px, vec![255, 0, 0, 255, 0, 0, 255, 255]);
        let packed = decode_packed(&enc).unwrap();
        assert_eq!(&packed[..8], &[2, 0, 0, 0, 1, 0, 0, 0]);
        assert!(decode_rgba(b"not an image").is_none());
    }

    #[test]
    fn asset_paths_stay_inside_the_root() {
        let r = Path::new("/game/public");
        assert_eq!(asset_path(r, "/assets/a.webp").unwrap(), Path::new("/game/public/assets/a.webp"));
        assert_eq!(asset_path(r, "pbr.json").unwrap(), Path::new("/game/public/pbr.json"));
        assert!(asset_path(r, "../secret").is_none());
        assert!(asset_path(r, "/a/../../b").is_none());
        assert!(asset_path(r, "").is_none());
    }
}
