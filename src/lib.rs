//! fastrender as a library: the portable core (protocol decoder, GLSL -> MSL translator, ring layouts, voxel
//! mesher) and, on macOS, the Metal renderer. The `fastrender` binary (src/main.rs) and the Electron addon
//! (host/electron/native) both link it.
//!
//! Hosts feed the renderer through [`host::FrameSource`]: per frame the command words (docs/PROTOCOL.md), the
//! resource messages (uploads / releases) written before them and the programs `host.program` translated.
//! JavaScriptCore (`jsc::ScriptHost`, in-process) and Electron (`host::Feed`, filled from the shared-memory
//! ring by the addon's render thread) are the two implementations.
pub mod core;
pub mod voxel;
pub mod script;
pub mod three;
pub mod xlat;
pub mod ring; // v3 per-slot ring layout + bounds-checked writes (src/mac_three.rs)
pub mod image; // __fr.readFile / __fr.decodeImage for real web games (JavaScriptCore host)
pub mod host; // FrameSource (host -> renderer boundary) + the Electron wire codec
#[cfg(target_os = "macos")]
pub mod jsc;
#[cfg(target_os = "macos")]
pub mod mac_three; // + its child modules: mac_targets.rs, mac_upload.rs, mac_shadow.rs, mac_bind.rs
#[cfg(target_os = "macos")]
pub mod dfg_lut;
#[cfg(target_os = "macos")]
pub mod mac_programs;
#[cfg(target_os = "macos")]
pub mod mac; // + its child module mac_fx.rs (MetalFX, internal-resolution targets)
#[cfg(target_os = "macos")]
pub mod app;
