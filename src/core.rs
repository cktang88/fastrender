//! Platform-independent parts: config, procedural test city, camera path, culling math,
//! frame statistics and the dynamic-resolution controller. Unit-tested on any OS.

use glam::{Mat4, Vec2, Vec3, Vec4};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderPath {
    /// one pass: geometry shaded directly (sun + shadow + brute-force point-light loop)
    Forward,
    /// one render pass: G-buffer in tile memory, read back with programmable blending,
    /// light volumes and tonemap in the same pass (Apple TBDR "single-pass deferred")
    Deferred,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SceneKind {
    /// procedural Manhattan grid of instanced boxes (GPU-culled, one indirect draw)
    City,
    /// voxel island meshed natively by voxel-castle-defense's greedy mesher (one draw per chunk)
    Voxel,
    /// a JS game script (three.js-shaped API, js/three-shim.js) running in JavaScriptCore
    Script,
    /// a real three.js game bundle using js/fastrender-three.js (protocol v2), in JavaScriptCore
    Three,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Upscale {
    /// bilinear full-screen pass from the internal resolution to the output
    Bilinear,
    /// MetalFX spatial scaler
    Spatial,
    /// MetalFX temporal scaler: sub-pixel jitter + depth-reprojected motion vectors (no per-shader
    /// velocity output needed). Falls back to `Spatial` where the device lacks it.
    Temporal,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub scene: SceneKind,
    /// game script for `--scene script`
    pub script: String,
    pub path: RenderPath,
    /// transient attachments (depth, G-buffer) memoryless (true) or stored to DRAM (false)
    pub memoryless: bool,
    /// internal resolution per axis, as a fraction of the output
    pub scale: f32,
    pub upscale: Upscale,
    /// 0 = shadow map cached (re-rendered only when the camera moves 40 m); n = every n frames
    pub shadow_every: u32,
    pub lights: u32,
    pub gpu_cull: bool,
    pub width: u32,
    pub height: u32,
    /// number of small prop boxes (12 triangles each) on top of the buildings
    pub props: u32,
    pub frames: u32,
    pub warmup: u32,
    /// bench: also run at least this many seconds (time-based runs for real hardware)
    pub seconds: f32,
    /// bench: also warm up at least this many seconds (DVFS settling)
    pub warmup_s: f32,
    pub vsync: bool,
    /// dynamic resolution target in ms (0 = off), windowed mode only
    pub dynres_ms: f32,
    /// print fps every n seconds of the run (bench mode, for thermal-throttling runs)
    pub log_every_s: f32,
    pub seed: u64,
    /// matrix: rounds over all rows (interleaved, so heat affects every row alike); median reported
    pub rounds: u32,
    /// bench mode: save the final frame as PNG
    pub shot: Option<String>,
    /// render every frame at this time (golden images): camera and animation frozen
    pub fixed_t: Option<f32>,
    /// three.js scenes: "three" (three's own programs, translated to MSL; the default) or "native" (the
    /// hand-written `fs_three`, protocol v2). Passed to the bundle as `__fr.params.programs`.
    pub programs: String,
    /// per-pass GPU ms (targets, shadow, main, upscale): the frame is committed as one command buffer per pass group
    /// (`--gpu-passes 1`; key G in the window). Off by default: three more commits per frame.
    pub gpu_passes: bool,
    /// frames-in-flight back-pressure: completion-handler counter (default) or an `MTLSharedEvent` the GPU signals
    /// (`--fence event`: the shape Metal 4's `MTL4CommandQueue.signalEvent` needs)
    pub fence: Fence,
    /// follow `NSProcessInfo.thermalState` / Low Power Mode: lower the scale ceiling and cap the frame rate
    /// (`--thermal 0` to ignore them, e.g. for benchmarks)
    pub thermal: bool,
    /// windowed mode: pace frames with `CAMetalDisplayLink` (macOS 14+) when vsync is on (`--display-link 0`: the
    /// `nextDrawable` loop)
    pub display_link: bool,
    /// windowed mode: exit after this many seconds and print the frame-loop report (0 = run until closed)
    pub run_seconds: f32,
}

/// What counts frames in flight (`mac::FrameFence`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fence {
    Cpu,
    Event,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            scene: SceneKind::City,
            script: "js/examples/boxes.js".into(),
            path: RenderPath::Deferred,
            memoryless: true,
            scale: 0.5,
            upscale: Upscale::Spatial,
            shadow_every: 0,
            lights: 64,
            gpu_cull: true,
            // the 13.6" Air's default "looks like 1470x956" mode is backed by 2940x1912 pixels
            width: 2940,
            height: 1912,
            props: 150_000,
            frames: 300,
            warmup: 30,
            seconds: 0.0,
            warmup_s: 0.0,
            vsync: true,
            dynres_ms: 0.0,
            log_every_s: 0.0,
            seed: 20261115,
            rounds: 3,
            shot: None,
            fixed_t: None,
            programs: "three".into(),
            gpu_passes: false,
            fence: Fence::Cpu,
            thermal: true,
            display_link: true,
            run_seconds: 0.0,
        }
    }
}

impl Config {
    pub fn label(&self) -> String {
        format!(
            "{} {} {} scale={:.2} up={} shadow={} lights={} cull={} props={}",
            match self.scene { SceneKind::City => "city", SceneKind::Voxel => "voxel", SceneKind::Script => "script", SceneKind::Three => "three" },
            match self.path { RenderPath::Forward => "forward", RenderPath::Deferred => "deferred" },
            if self.memoryless { "memoryless" } else { "stored" },
            self.scale,
            match self.upscale { Upscale::Bilinear => "bilinear", Upscale::Spatial => "metalfx", Upscale::Temporal => "temporal" },
            if self.shadow_every == 0 { "cached".to_string() } else { format!("every{}", self.shadow_every) },
            self.lights,
            if self.gpu_cull { "gpu" } else { "off" },
            self.props,
        )
    }

    /// parse `--key value` pairs on top of `self`; returns an error for unknown keys
    pub fn apply_args(&mut self, args: &[String]) -> Result<(), String> {
        let mut i = 0;
        while i < args.len() {
            let k = args[i].trim_start_matches("--");
            let v = args.get(i + 1).ok_or_else(|| format!("missing value for --{k}"))?;
            let num = |s: &str| s.parse::<f64>().map_err(|_| format!("bad number for --{k}: {s}"));
            match k {
                "scene" => self.scene = match v.as_str() { "city" => SceneKind::City, "voxel" => SceneKind::Voxel, "script" => SceneKind::Script, _ => return Err(format!("bad --scene {v}")) },
                "script" => { self.script = v.clone(); self.scene = SceneKind::Script; }
                "three" => { self.script = v.clone(); self.scene = SceneKind::Three; }
                "path" => self.path = match v.as_str() { "forward" => RenderPath::Forward, "deferred" => RenderPath::Deferred, _ => return Err(format!("bad --path {v}")) },
                "memoryless" => self.memoryless = num(v)? != 0.0,
                "scale" => self.scale = (num(v)? as f32).clamp(0.25, 1.0),
                "upscale" => self.upscale = match v.as_str() { "bilinear" | "none" => Upscale::Bilinear, "metalfx" | "spatial" => Upscale::Spatial, "temporal" => Upscale::Temporal, _ => return Err(format!("bad --upscale {v}")) },
                "shadow-every" => self.shadow_every = num(v)? as u32,
                "lights" => self.lights = (num(v)? as u32).min(MAX_LIGHTS),
                "cull" => self.gpu_cull = num(v)? != 0.0,
                "size" => {
                    let (w, h) = v.split_once('x').ok_or("--size WxH")?;
                    self.width = w.parse().map_err(|_| "bad width")?;
                    self.height = h.parse().map_err(|_| "bad height")?;
                }
                "props" => self.props = num(v)? as u32,
                "frames" => self.frames = num(v)? as u32,
                "warmup" => self.warmup = num(v)? as u32,
                "seconds" => self.seconds = num(v)? as f32,
                "warmup-seconds" => self.warmup_s = num(v)? as f32,
                "vsync" => self.vsync = num(v)? != 0.0,
                "dynres" => self.dynres_ms = num(v)? as f32,
                "log-every" => self.log_every_s = num(v)? as f32,
                "seed" => self.seed = num(v)? as u64,
                "rounds" => self.rounds = (num(v)? as u32).max(1),
                "shot" => self.shot = Some(v.clone()),
                "time" => self.fixed_t = Some(num(v)? as f32),
                "programs" => self.programs = match v.as_str() { "three" | "native" => v.clone(), _ => return Err(format!("bad --programs {v} (three|native)")) },
                "gpu-passes" => self.gpu_passes = num(v)? != 0.0,
                "fence" => self.fence = match v.as_str() { "cpu" | "semaphore" => Fence::Cpu, "event" => Fence::Event, _ => return Err(format!("bad --fence {v} (cpu|event)")) },
                "thermal" => self.thermal = num(v)? != 0.0,
                "display-link" => self.display_link = num(v)? != 0.0,
                "run-seconds" => self.run_seconds = num(v)? as f32,
                _ => return Err(format!("unknown option --{k}")),
            }
            i += 2;
        }
        Ok(())
    }
}

pub const MAX_LIGHTS: u32 = 1024;

/// GPU instance: an oriented box. Matches `Instance` in shaders/fastrender.metal.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Instance {
    /// xyz centre, w yaw (radians)
    pub a: [f32; 4],
    /// xyz half extents, w packed RGBA8 albedo (bit pattern)
    pub b: [f32; 4],
}

/// Point light. Matches `Light` in the shader.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Light {
    pub pos_radius: [f32; 4],
    pub color: [f32; 4],
}

/// Per-frame uniforms. Matches `Frame` in the shader (all 16-byte aligned).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FrameUniforms {
    pub view_proj: [[f32; 4]; 4],
    pub inv_view_proj: [[f32; 4]; 4],
    pub shadow_vp: [[f32; 4]; 4],
    pub cam_pos: [f32; 4],
    pub sun_dir: [f32; 4],
    pub fog: [f32; 4],
    pub frustum: [[f32; 4]; 6],
    /// x = instances, y = lights, z/w unused
    pub counts: [u32; 4],
    /// xy = content size in pixels, zw = allocated target size
    pub viewport: [f32; 4],
    /// fog for three.js scenes: near (or density), far, mode (0 exp [city/voxel], 1 linear, 2 exp2), 0
    pub fog2: [f32; 4],
}

pub fn pack_rgba(r: f32, g: f32, b: f32) -> f32 {
    let q = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u32;
    f32::from_bits(q(r) | (q(g) << 8) | (q(b) << 16) | (255 << 24))
}

/// small deterministic RNG (splitmix64)
pub struct Rng(u64);
impl Rng {
    pub fn new(seed: u64) -> Self { Rng(seed) }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    pub fn f32(&mut self) -> f32 { (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32 }
    pub fn range(&mut self, a: f32, b: f32) -> f32 { a + (b - a) * self.f32() }
}

pub struct Scene {
    pub instances: Vec<Instance>,
    pub lights: Vec<Light>,
    /// half size of the city square, metres
    pub extent: f32,
}

pub const BLOCK_X: f32 = 80.0;
pub const BLOCK_Z: f32 = 60.0;
pub const STREET: f32 = 20.0;
pub const BLOCKS: i32 = 14;

/// A Manhattan-like grid: street-wall buildings per block, a ground slab, and `props` small boxes
/// (window frames, AC units, bollards...) on facades and sidewalks to push the triangle count.
pub fn build_city(props: u32, lights: u32, seed: u64) -> Scene {
    let mut rng = Rng::new(seed);
    let mut inst = Vec::new();
    let pitch_x = BLOCK_X + STREET;
    let pitch_z = BLOCK_Z + STREET;
    let extent = BLOCKS as f32 * pitch_x.max(pitch_z) * 0.5;
    // ground
    inst.push(Instance { a: [0.0, -0.5, 0.0, 0.0], b: [extent + 50.0, 0.5, extent + 50.0, pack_rgba(0.32, 0.32, 0.34)] });
    let mut facades: Vec<(Vec3, Vec3, f32)> = Vec::new(); // (centre, half, facing yaw) of street faces
    for bx in -BLOCKS / 2..BLOCKS / 2 {
        for bz in -BLOCKS / 2..BLOCKS / 2 {
            let cx = (bx as f32 + 0.5) * pitch_x;
            let cz = (bz as f32 + 0.5) * pitch_z;
            // sidewalk slab
            inst.push(Instance { a: [cx, 0.08, cz, 0.0], b: [BLOCK_X * 0.5 + 3.0, 0.08, BLOCK_Z * 0.5 + 3.0, pack_rgba(0.55, 0.54, 0.52)] });
            // buildings along the two long (x) frontages
            for side in [-1.0f32, 1.0] {
                let mut x = -BLOCK_X * 0.5;
                while x < BLOCK_X * 0.5 - 4.0 {
                    let w = rng.range(7.0, 16.0).min(BLOCK_X * 0.5 - x);
                    let d = rng.range(12.0, BLOCK_Z * 0.5);
                    let h = if rng.f32() < 0.08 { rng.range(60.0, 160.0) } else { rng.range(12.0, 35.0) };
                    let tone = rng.range(0.35, 0.75);
                    let warm = rng.range(0.85, 1.1);
                    let centre = Vec3::new(cx + x + w * 0.5, h * 0.5, cz + side * (BLOCK_Z * 0.5 - d * 0.5));
                    let half = Vec3::new(w * 0.5, h * 0.5, d * 0.5);
                    inst.push(Instance { a: [centre.x, centre.y, centre.z, 0.0], b: [half.x, half.y, half.z, pack_rgba(tone * warm, tone * 0.9, tone / warm)] });
                    facades.push((centre + Vec3::new(0.0, 0.0, side * half.z), half, side));
                    x += w;
                }
            }
        }
    }
    // props: mostly window frames on facades (thin boxes), some street furniture
    for _ in 0..props {
        if facades.is_empty() { break; }
        let (c, half, side) = facades[(rng.next_u64() % facades.len() as u64) as usize];
        if rng.f32() < 0.85 {
            let px = c.x + rng.range(-half.x + 1.0, half.x - 1.0);
            let py = rng.range(3.0, half.y * 2.0 - 1.0);
            inst.push(Instance { a: [px, py, c.z + side * 0.12, 0.0], b: [0.6, 0.8, 0.12, pack_rgba(0.12, 0.14, 0.18)] });
        } else {
            let px = c.x + rng.range(-half.x, half.x);
            let pz = c.z + side * rng.range(1.0, 3.5);
            let s = rng.range(0.2, 0.7);
            inst.push(Instance { a: [px, s, pz, rng.range(0.0, 6.28)], b: [s, s, s * 0.6, pack_rgba(rng.f32(), rng.f32(), rng.f32())] });
        }
    }
    // street lights: a lamp every 25 m on both kerbs of every north-south street, nearest to the
    // camera's avenue first, so the first N lights are the ones the camera actually sees
    // (review finding: lights placed column by column were all 400-800 m away)
    let (avenue_x, _) = camera_at(0.0);
    let mut cands: Vec<Vec3> = Vec::new();
    for sx in -BLOCKS / 2..=BLOCKS / 2 {
        let x = sx as f32 * pitch_x;
        for side in [-1.0f32, 1.0] {
            let mut z = -extent + 12.0;
            while z < extent {
                cands.push(Vec3::new(x + side * (STREET * 0.5 - 1.0), 5.0, z + if side > 0.0 { 12.5 } else { 0.0 }));
                z += 25.0;
            }
        }
    }
    cands.sort_by(|a, b| ((a.x - avenue_x.x).abs(), a.z.abs()).partial_cmp(&((b.x - avenue_x.x).abs(), b.z.abs())).unwrap());
    let ls: Vec<Light> = cands
        .iter()
        .take(lights as usize)
        .enumerate()
        .map(|(i, p)| {
            let hue = (i % 3) as f32;
            Light { pos_radius: [p.x, p.y, p.z, 18.0], color: [3.0 + hue * 0.2, 2.2, 1.2 + hue * 0.3, 0.0] }
        })
        .collect();
    Scene { instances: inst, lights: ls, extent }
}

/// Deterministic street-level camera path: down an avenue, turn, across a street.
pub fn camera_at(t: f32) -> (Vec3, Vec3) {
    let pitch_x = BLOCK_X + STREET;
    let speed = 9.0; // m/s, a slow drive
    let len = BLOCKS as f32 * pitch_x * 0.8;
    let s = (t * speed) % (2.0 * len);
    let avenue_x = pitch_x; // streets are centred on multiples of the block pitch
    let (pos, dir) = if s < len {
        (Vec3::new(avenue_x, 1.7, -len * 0.5 + s), Vec3::new((t * 0.3).sin() * 0.35, -0.03, 1.0))
    } else {
        (Vec3::new(avenue_x, 1.7, len * 0.5 - (s - len)), Vec3::new((t * 0.3).sin() * 0.35, 0.05, -1.0))
    };
    (pos, dir.normalize())
}

/// Gribb-Hartmann frustum planes (normalised, pointing inward) from a Metal-style
/// (z in [0,1]) view-projection matrix.
pub fn frustum_planes(vp: &Mat4) -> [Vec4; 6] {
    let r0 = vp.row(0);
    let r1 = vp.row(1);
    let r2 = vp.row(2);
    let r3 = vp.row(3);
    let mut p = [r3 + r0, r3 - r0, r3 + r1, r3 - r1, r2, r3 - r2];
    for q in p.iter_mut() {
        *q /= q.truncate().length();
    }
    p
}

pub fn sphere_visible(planes: &[Vec4; 6], c: Vec3, r: f32) -> bool {
    planes.iter().all(|p| p.truncate().dot(c) + p.w >= -r)
}

/// Frame-time statistics (milliseconds).
#[derive(Default, Clone, Debug)]
pub struct Stats {
    pub samples: Vec<f64>,
}
impl Stats {
    pub fn push(&mut self, ms: f64) { self.samples.push(ms); }
    pub fn avg(&self) -> f64 {
        if self.samples.is_empty() { return 0.0; }
        self.samples.iter().sum::<f64>() / self.samples.len() as f64
    }
    pub fn pct(&self, p: f64) -> f64 {
        if self.samples.is_empty() { return 0.0; }
        let mut s = self.samples.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let idx = ((p / 100.0) * (s.len() - 1) as f64).round() as usize;
        s[idx.min(s.len() - 1)]
    }
}

/// Dynamic resolution: moves the render scale so GPU time sits just under the target.
/// Smoothed (EMA), with a dead band and a cooldown so it doesn't oscillate.
pub struct DynRes {
    pub target_ms: f32,
    pub scale: f32,
    pub min: f32,
    pub max: f32,
    ema: f32,
    cooldown: u32,
}
impl DynRes {
    pub fn new(target_ms: f32, start: f32) -> Self {
        DynRes { target_ms, scale: start, min: 0.35, max: 1.0, ema: target_ms, cooldown: 0 }
    }
    /// New floor and ceiling (thermal state, Low Power Mode: `PowerState::limits`); returns true when the current
    /// scale had to move to stay inside them. A raised ceiling is climbed by `update` at its usual pace.
    pub fn set_limits(&mut self, l: PowerLimits) -> bool {
        self.min = l.min_scale.min(l.max_scale);
        self.max = l.max_scale;
        let new = self.scale.clamp(self.min, self.max);
        let changed = (new - self.scale).abs() > 1e-4;
        self.scale = new;
        if changed {
            self.cooldown = 20;
        }
        changed
    }
    /// feed one GPU frame time; returns true when the scale changed
    pub fn update(&mut self, gpu_ms: f32) -> bool {
        self.ema = self.ema * 0.9 + gpu_ms * 0.1;
        if self.cooldown > 0 {
            self.cooldown -= 1;
            return false;
        }
        let ratio = self.ema / self.target_ms;
        let new = if ratio > 1.0 {
            // GPU cost scales ~ with pixel count = scale^2
            (self.scale / ratio.sqrt()).max(self.scale - 0.1)
        } else if ratio < 0.8 {
            (self.scale + 0.05).min(self.scale / ratio.sqrt())
        } else {
            self.scale
        };
        let new = (new.clamp(self.min, self.max) * 40.0).round() / 40.0;
        if (new - self.scale).abs() > 1e-4 {
            self.scale = new;
            self.cooldown = 20;
            true
        } else {
            false
        }
    }
}

// ------------------------------------------------------------------ thermal state and Low Power Mode
//
// A fanless Air has no way to shed heat but to do less. `NSProcessInfo.thermalState` and Low Power Mode are read by
// the hosts about once a second (`mac::power_state`) and turned into limits here (pure, unit-tested):
// - fair: dynamic resolution may go lower (nothing is taken away yet);
// - serious: scale ceiling 0.75 (44 % fewer pixels than native) and a 30 fps cap;
// - critical: scale ceiling 0.5 and a 30 fps cap;
// - Low Power Mode: scale ceiling 0.75; no cap of our own (the system lowers the display's refresh itself).

/// `NSProcessInfoThermalState`
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Thermal {
    #[default]
    Nominal,
    Fair,
    Serious,
    Critical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct PowerState {
    pub thermal: Thermal,
    pub low_power: bool,
}

/// What a power state allows: the range of the internal-resolution scale and the frame-rate cap.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PowerLimits {
    pub min_scale: f32,
    pub max_scale: f32,
    pub fps_cap: Option<f32>,
}

impl PowerState {
    /// From `FASTRENDER_THERMAL=nominal|fair|serious|critical` and `FASTRENDER_LOW_POWER=1` (to try the policy by
    /// hand and on CI, where the real state never leaves nominal).
    pub fn from_env() -> Option<PowerState> {
        let thermal = match std::env::var("FASTRENDER_THERMAL").ok().as_deref() {
            Some("nominal") => Some(Thermal::Nominal),
            Some("fair") => Some(Thermal::Fair),
            Some("serious") => Some(Thermal::Serious),
            Some("critical") => Some(Thermal::Critical),
            _ => None,
        };
        let low_power = std::env::var("FASTRENDER_LOW_POWER").ok().map(|v| v != "0");
        if thermal.is_none() && low_power.is_none() {
            return None;
        }
        Some(PowerState { thermal: thermal.unwrap_or_default(), low_power: low_power.unwrap_or(false) })
    }

    pub fn limits(self) -> PowerLimits {
        let (min_scale, mut max_scale, fps_cap): (f32, f32, Option<f32>) = match self.thermal {
            Thermal::Nominal => (0.35, 1.0, None),
            Thermal::Fair => (0.3, 1.0, None),
            Thermal::Serious => (0.3, 0.75, Some(30.0)),
            Thermal::Critical => (0.25, 0.5, Some(30.0)),
        };
        if self.low_power {
            max_scale = max_scale.min(0.75);
        }
        PowerLimits { min_scale, max_scale, fps_cap }
    }

    pub fn label(self) -> String {
        format!("thermal {}{}", match self.thermal { Thermal::Nominal => "nominal", Thermal::Fair => "fair", Thermal::Serious => "serious", Thermal::Critical => "critical" }, if self.low_power { ", low power" } else { "" })
    }
}

// ------------------------------------------------------------------ frame-loop and per-pass GPU statistics

/// Pass groups of a frame, in the order they are encoded. Per-pass GPU time is reported per group.
pub const GPU_PASSES: [&str; 4] = ["targets", "shadow", "main", "upscale"];
/// offscreen render-target passes (PMREM, post-processing chains: src/mac_targets.rs)
pub const PASS_TARGETS: usize = 0;
/// every shadow map of the frame (and the shadow cull)
pub const PASS_SHADOW: usize = 1;
/// camera cull + the main pass
pub const PASS_MAIN: usize = 2;
/// motion vectors + MetalFX / bilinear upscale (nothing at scale 1)
pub const PASS_UPSCALE: usize = 3;

/// GPU ms per pass group, averaged (`mac::Renderer::take_pass_times`).
#[derive(Clone, Debug, Default)]
pub struct PassStats {
    pub frames: u64,
    pub sum_ms: [f64; 4],
}
impl PassStats {
    pub fn push(&mut self, ms: [f64; 4]) {
        self.frames += 1;
        for i in 0..4 { self.sum_ms[i] += ms[i]; }
    }
    pub fn avg(&self) -> [f64; 4] { self.sum_ms.map(|s| if self.frames == 0 { 0.0 } else { s / self.frames as f64 }) }
    /// "targets 0.12, shadow 1.30, main 4.21, upscale 0.80"
    pub fn line(&self) -> String {
        let a = self.avg();
        (0..4).map(|i| format!("{} {:.2}", GPU_PASSES[i], a[i])).collect::<Vec<_>>().join(", ")
    }
}

/// How the frame loop ran: pipelining depth and where the render thread blocked.
#[derive(Clone, Debug, Default)]
pub struct LoopStats {
    pub frames: u64,
    /// frames in flight at each commit, the committed one included: 1 = the GPU was idle (CPU-bound), the limit =
    /// the GPU is the bottleneck
    pub in_flight_sum: u64,
    pub in_flight_max: u32,
    /// time blocked waiting for a frame slot (back-pressure: by design, only when GPU-bound)
    pub fence_wait_ms: f64,
    pub fence_wait_max_ms: f64,
    /// frames that waited more than 1 ms for a slot
    pub fence_stalls: u64,
    /// time inside the drawable callback (`nextDrawable`; ~0 with a display link, which hands the drawable over)
    pub drawable_wait_ms: f64,
    pub drawable_wait_max_ms: f64,
}
impl LoopStats {
    pub fn commit(&mut self, in_flight: u32, fence_wait_ms: f64, drawable_wait_ms: f64) {
        self.frames += 1;
        self.in_flight_sum += in_flight as u64;
        self.in_flight_max = self.in_flight_max.max(in_flight);
        self.fence_wait_ms += fence_wait_ms;
        self.fence_wait_max_ms = self.fence_wait_max_ms.max(fence_wait_ms);
        if fence_wait_ms > 1.0 { self.fence_stalls += 1; }
        self.drawable_wait_ms += drawable_wait_ms;
        self.drawable_wait_max_ms = self.drawable_wait_max_ms.max(drawable_wait_ms);
    }
    pub fn merge(&mut self, o: &LoopStats) {
        self.frames += o.frames;
        self.in_flight_sum += o.in_flight_sum;
        self.in_flight_max = self.in_flight_max.max(o.in_flight_max);
        self.fence_wait_ms += o.fence_wait_ms;
        self.fence_wait_max_ms = self.fence_wait_max_ms.max(o.fence_wait_max_ms);
        self.fence_stalls += o.fence_stalls;
        self.drawable_wait_ms += o.drawable_wait_ms;
        self.drawable_wait_max_ms = self.drawable_wait_max_ms.max(o.drawable_wait_max_ms);
    }
    pub fn line(&self, limit: usize) -> String {
        let n = self.frames.max(1) as f64;
        format!(
            "frames in flight {:.2} avg, {} max (limit {limit}); slot wait {:.3} ms avg, {:.2} max, {} of {} frames > 1 ms; drawable wait {:.3} ms avg, {:.2} max",
            self.in_flight_sum as f64 / n, self.in_flight_max, self.fence_wait_ms / n, self.fence_wait_max_ms, self.fence_stalls, self.frames, self.drawable_wait_ms / n, self.drawable_wait_max_ms
        )
    }
}

// ------------------------------------------------------------------ temporal upscaling (MetalFX)
//
// Pure maths, mirrored by the `motion_from_depth` kernel in shaders/fastrender.metal and unit-tested
// below. Conventions (docs/notes/metalfx-temporal.md):
// - pixel coordinates: origin top-left, y down, pixel centres at +0.5 (Metal window coordinates);
// - jitter `j` (pixels): the rasterised image is shifted by +j, i.e. a clip-space translation of
//   (+2 jx / w, -2 jy / h). The same `j` goes to MTLFXTemporalScaler's `jitterOffsetX/Y` (the
//   FSR2/DLSS convention, which MetalFX integrations such as Godot's pass straight through);
// - motion (UV units of the current content rect): `prev_uv - cur_uv`, both from *unjittered*
//   matrices; `motionVectorScale = content size` turns it into pixels. Apple's definition: an object
//   that moved 10 px down and right this frame has motion (-10, -10).

/// Number of jitter phases. 16 Halton(2,3) points cover a pixel well at 1.5-2x upscale; more phases
/// (DLSS suggests 8 x factor^2) only help static images and slow convergence after motion.
pub const JITTER_PHASES: u64 = 16;

/// Radical inverse of `i` in `base` (Halton sequence), in [0, 1).
pub fn halton(mut i: u32, base: u32) -> f32 {
    let (mut f, mut r) = (1.0f64, 0.0f64);
    while i > 0 {
        f /= base as f64;
        r += f * (i % base) as f64;
        i /= base;
    }
    r as f32
}

/// Sub-pixel jitter for `frame`, in pixels, each component in [-0.5, 0.5). Index 0 of the Halton
/// sequence is skipped (it is 0 in both bases).
pub fn jitter_px(frame: u64) -> Vec2 {
    let i = (frame % JITTER_PHASES) as u32 + 1;
    Vec2::new(halton(i, 2) - 0.5, halton(i, 3) - 0.5)
}

/// Apply a sub-pixel jitter to a projection (or view-projection) matrix: a clip-space translation of
/// (+2 jx / w, -2 jy / h) (times w), which shifts the rasterised image by `jitter_px` pixels
/// (x right, y down) on a render target of `size` = (w, h) pixels (the *content* size with dynamic
/// resolution).
///
/// It works on any matrix whose output is clip space, OpenGL or Metal convention alike (only x and y
/// change), so it applies equally to `proj`, `proj * view`, or three.js's `projectionMatrix` before
/// or after `three::gl_to_metal`. **A path that packs `projectionMatrix` into a uniform block for
/// three's own translated GLSL shaders must pack `jittered(projectionMatrix, j, content_size)`**, with
/// `j` = `Renderer::jitter()` for that frame, or MetalFX reconstructs a blurred, wobbling image.
/// (Leave `modelViewMatrix`/`viewMatrix`/`cameraPosition` unjittered.)
pub fn jittered(m: Mat4, jitter_px: Vec2, size: (f32, f32)) -> Mat4 {
    Mat4::from_translation(Vec3::new(2.0 * jitter_px.x / size.0, -2.0 * jitter_px.y / size.1, 0.0)) * m
}

/// Rust mirror of the `motion_from_depth` kernel: the motion vector (UV of the content rect,
/// `prev - cur`) of pixel `px` whose depth-buffer value is `depth`.
/// `inv_cur_jittered` = inverse of the jittered view-projection the frame was rasterised with
/// (it recovers the surface point the pixel centre sampled); `cur`/`prev` = the unjittered
/// view-projections of this frame and the previous one (so the motion carries no jitter).
pub fn reproject_motion(inv_cur_jittered: &Mat4, cur: &Mat4, prev: &Mat4, px: (u32, u32), depth: f32, content: (f32, f32)) -> Vec2 {
    let uv = Vec2::new((px.0 as f32 + 0.5) / content.0, (px.1 as f32 + 0.5) / content.1);
    let ndc = Vec2::new(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    let w = *inv_cur_jittered * Vec4::new(ndc.x, ndc.y, depth, 1.0);
    let w = w / w.w;
    let to_uv = |c: Vec4| Vec2::new(c.x / c.w * 0.5 + 0.5, 0.5 - c.y / c.w * 0.5);
    let c = *cur * w;
    let p = *prev * w;
    if p.w <= 1e-6 || c.w <= 1e-6 {
        return Vec2::ZERO; // behind the previous camera: no usable history
    }
    to_uv(p) - to_uv(c)
}

/// A camera jump this large in one frame (teleport, path wrap-around) is a cut: reset the history.
pub fn is_camera_cut(prev_eye: Vec3, eye: Vec3) -> bool { prev_eye.distance(eye) > 10.0 }

/// The benchmark matrix: each entry tests one hypothesis against its neighbour.
pub fn bench_matrix(base: &Config) -> Vec<(String, Config)> {
    let mut v = Vec::new();
    let mk = |f: &dyn Fn(&mut Config)| { let mut c = base.clone(); f(&mut c); c };
    use RenderPath::*;
    use Upscale::*;
    v.push(("A naive: forward, stored, native res".into(), mk(&|c| { c.path = Forward; c.memoryless = false; c.scale = 1.0; c.upscale = Bilinear; c.lights = 0; })));
    v.push(("B forward + memoryless depth".into(), mk(&|c| { c.path = Forward; c.memoryless = true; c.scale = 1.0; c.upscale = Bilinear; c.lights = 0; })));
    v.push(("C deferred stored (G-buffer to DRAM)".into(), mk(&|c| { c.path = Deferred; c.memoryless = false; c.scale = 1.0; c.upscale = Bilinear; c.lights = 0; })));
    v.push(("D deferred memoryless (tile G-buffer)".into(), mk(&|c| { c.path = Deferred; c.memoryless = true; c.scale = 1.0; c.upscale = Bilinear; c.lights = 0; })));
    v.push(("E forward 0.5x + bilinear".into(), mk(&|c| { c.path = Forward; c.scale = 0.5; c.upscale = Bilinear; c.lights = 0; })));
    v.push(("F forward 0.5x + MetalFX".into(), mk(&|c| { c.path = Forward; c.scale = 0.5; c.upscale = Spatial; c.lights = 0; })));
    v.push(("G deferred 0.5x + MetalFX".into(), mk(&|c| { c.path = Deferred; c.scale = 0.5; c.upscale = Spatial; c.lights = 0; })));
    v.push(("H forward 0.5x, 32 lights (loop)".into(), mk(&|c| { c.path = Forward; c.scale = 0.5; c.upscale = Spatial; c.lights = 32; })));
    v.push(("I deferred 0.5x, 32 lights (volumes)".into(), mk(&|c| { c.path = Deferred; c.scale = 0.5; c.upscale = Spatial; c.lights = 32; })));
    v.push(("J deferred 0.5x, 256 lights".into(), mk(&|c| { c.path = Deferred; c.scale = 0.5; c.upscale = Spatial; c.lights = 256; })));
    v.push(("K shadow every frame (vs G)".into(), mk(&|c| { c.path = Deferred; c.scale = 0.5; c.upscale = Spatial; c.lights = 0; c.shadow_every = 1; })));
    v.push(("L no culling at all: frustum + prop distance off (vs G)".into(), mk(&|c| { c.path = Deferred; c.scale = 0.5; c.upscale = Spatial; c.lights = 0; c.gpu_cull = false; })));
    v.push(("M forward 0.5x + MetalFX temporal (vs F)".into(), mk(&|c| { c.path = Forward; c.scale = 0.5; c.upscale = Temporal; c.lights = 0; })));
    v.push(("N forward 0.67x + MetalFX temporal (vs B, M)".into(), mk(&|c| { c.path = Forward; c.scale = 0.67; c.upscale = Temporal; c.lights = 0; })));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn city_is_deterministic_and_sized() {
        let a = build_city(1000, 64, 7);
        let b = build_city(1000, 64, 7);
        assert_eq!(a.instances.len(), b.instances.len());
        assert_eq!(a.instances[100], b.instances[100]);
        assert_eq!(a.lights.len(), 64);
        assert!(a.instances.len() > 1000 + BLOCKS as usize * BLOCKS as usize * 4);
        assert_eq!(std::mem::size_of::<Instance>(), 32);
        assert_eq!(std::mem::size_of::<FrameUniforms>() % 16, 0);
        assert_eq!(std::mem::size_of::<FrameUniforms>(), 3 * 64 + 3 * 16 + 6 * 16 + 16 + 16 + 16);
    }

    #[test]
    fn lights_are_where_the_camera_drives() {
        let s = build_city(0, 64, 1);
        let (eye, _) = camera_at(0.0);
        let near = s.lights.iter().filter(|l| (l.pos_radius[0] - eye.x).abs() < 15.0).count();
        assert!(near >= 48, "only {near} of 64 lights line the camera's avenue");
    }

    #[test]
    fn camera_stays_in_city() {
        let s = build_city(0, 0, 1);
        for i in 0..2000 {
            let (p, d) = camera_at(i as f32 * 0.1);
            assert!(p.x.abs() < s.extent && p.z.abs() < s.extent, "camera left the city at {p}");
            assert!((d.length() - 1.0).abs() < 1e-4);
            // not inside a block (blocks + sidewalks span +-(BLOCK_X/2 + 3) around (k+0.5)*pitch)
            let pitch_x = BLOCK_X + STREET;
            let local = (p.x / pitch_x).rem_euclid(1.0) * pitch_x - pitch_x * 0.5;
            assert!(local.abs() > BLOCK_X * 0.5 + 3.0, "camera at x={} is inside a block", p.x);
        }
    }

    #[test]
    fn frustum_culls_behind_and_keeps_ahead() {
        let view = glam::camera::rh::view::look_to_mat4(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y);
        let proj = glam::camera::rh::proj::directx::perspective(1.2, 1.5, 0.1, 500.0); // glam rh = z in [0,1]
        let planes = frustum_planes(&(proj * view));
        assert!(sphere_visible(&planes, Vec3::new(0.0, 0.0, -10.0), 1.0));
        assert!(!sphere_visible(&planes, Vec3::new(0.0, 0.0, 10.0), 1.0));
        assert!(!sphere_visible(&planes, Vec3::new(0.0, 0.0, -600.0), 1.0));
        assert!(sphere_visible(&planes, Vec3::new(0.0, 0.0, 2.0), 2.5)); // straddles near plane
    }

    #[test]
    fn stats_percentiles() {
        let mut s = Stats::default();
        for i in 1..=100 { s.push(i as f64); }
        assert!((s.avg() - 50.5).abs() < 1e-9);
        assert_eq!(s.pct(50.0), 51.0);
        assert_eq!(s.pct(99.0), 99.0);
    }

    #[test]
    fn dynres_converges_and_holds() {
        // model: GPU ms = 40 * scale^2 (a frame that needs 40 ms at native res)
        let mut d = DynRes::new(16.0, 1.0);
        for _ in 0..2000 {
            let ms = 40.0 * d.scale * d.scale;
            d.update(ms);
        }
        let ms = 40.0 * d.scale * d.scale;
        assert!(ms <= 16.0 && ms > 16.0 * 0.6, "settled at scale {} -> {ms} ms", d.scale);
        // and it should not keep changing once settled
        let s = d.scale;
        let mut changes = 0;
        for _ in 0..500 { if d.update(40.0 * d.scale * d.scale) { changes += 1; } }
        assert!(changes <= 1 && (d.scale - s).abs() < 0.06);
    }

    #[test]
    fn power_limits_follow_the_thermal_state() {
        let l = |t, lp| PowerState { thermal: t, low_power: lp }.limits();
        assert_eq!(l(Thermal::Nominal, false), PowerLimits { min_scale: 0.35, max_scale: 1.0, fps_cap: None });
        assert_eq!(l(Thermal::Fair, false).max_scale, 1.0);
        assert!(l(Thermal::Fair, false).min_scale < 0.35);
        assert_eq!((l(Thermal::Serious, false).max_scale, l(Thermal::Serious, false).fps_cap), (0.75, Some(30.0)));
        assert_eq!((l(Thermal::Critical, false).max_scale, l(Thermal::Critical, false).fps_cap), (0.5, Some(30.0)));
        // Low Power Mode lowers the ceiling, never raises it, and adds no cap of its own
        assert_eq!((l(Thermal::Nominal, true).max_scale, l(Thermal::Nominal, true).fps_cap), (0.75, None));
        assert_eq!(l(Thermal::Critical, true).max_scale, 0.5);
        // monotonic: a hotter state never allows more
        let order = [Thermal::Nominal, Thermal::Fair, Thermal::Serious, Thermal::Critical];
        assert!(order.windows(2).all(|w| l(w[1], false).max_scale <= l(w[0], false).max_scale && l(w[1], false).min_scale <= l(w[0], false).min_scale));
    }

    #[test]
    fn dynres_obeys_power_limits() {
        // a cheap frame: DynRes sits at scale 1 until the state goes serious
        let mut d = DynRes::new(16.0, 1.0);
        for _ in 0..200 { d.update(4.0); }
        assert_eq!(d.scale, 1.0);
        assert!(d.set_limits(PowerState { thermal: Thermal::Serious, low_power: false }.limits()));
        assert_eq!(d.scale, 0.75);
        for _ in 0..500 { d.update(4.0 * d.scale * d.scale); }
        assert_eq!(d.scale, 0.75, "must not climb past the ceiling");
        // back to nominal: no jump, then it climbs on its own
        assert!(!d.set_limits(PowerState::default().limits()));
        for _ in 0..500 { d.update(4.0 * d.scale * d.scale); }
        assert_eq!(d.scale, 1.0);
        // critical while already low: unchanged scale, lower floor available
        let mut d = DynRes::new(16.0, 0.4);
        assert!(!d.set_limits(PowerState { thermal: Thermal::Critical, low_power: true }.limits()));
        assert_eq!((d.min, d.max), (0.25, 0.5));
    }

    #[test]
    fn loop_and_pass_stats() {
        let mut l = LoopStats::default();
        l.commit(1, 0.0, 0.2);
        l.commit(2, 3.0, 0.4);
        assert_eq!((l.frames, l.in_flight_max, l.fence_stalls), (2, 2, 1));
        let mut m = LoopStats::default();
        m.merge(&l);
        m.merge(&l);
        assert_eq!((m.frames, m.in_flight_sum, m.fence_wait_max_ms), (4, 6, 3.0));
        assert!(m.line(2).starts_with("frames in flight 1.50 avg, 2 max (limit 2)"));
        let mut p = PassStats::default();
        p.push([0.0, 1.0, 4.0, 1.0]);
        p.push([0.0, 3.0, 2.0, 1.0]);
        assert_eq!(p.avg(), [0.0, 2.0, 3.0, 1.0]);
        assert_eq!(p.line(), "targets 0.00, shadow 2.00, main 3.00, upscale 1.00");
    }

    #[test]
    fn frame_loop_options_parse() {
        let mut c = Config::default();
        assert!(!c.gpu_passes && c.fence == Fence::Cpu && c.thermal && c.display_link);
        let args: Vec<String> = "--gpu-passes 1 --fence event --thermal 0 --display-link 0 --run-seconds 2.5".split(' ').map(String::from).collect();
        c.apply_args(&args).unwrap();
        assert!(c.gpu_passes && c.fence == Fence::Event && !c.thermal && !c.display_link && c.run_seconds == 2.5);
        assert!(c.apply_args(&["--fence".into(), "nope".into()]).is_err());
    }

    #[test]
    fn halton_sequence_and_jitter() {
        let b2: Vec<f32> = (1..5).map(|i| halton(i, 2)).collect();
        assert_eq!(b2, vec![0.5, 0.25, 0.75, 0.125]);
        let b3: Vec<f32> = (1..5).map(|i| halton(i, 3)).collect();
        for (a, b) in b3.iter().zip([1.0 / 3.0, 2.0 / 3.0, 1.0 / 9.0, 4.0 / 9.0]) {
            assert!((a - b).abs() < 1e-6);
        }
        // jitter: in range, well spread, about zero-mean over a cycle, periodic
        let js: Vec<Vec2> = (0..JITTER_PHASES).map(jitter_px).collect();
        assert!(js.iter().all(|j| j.x >= -0.5 && j.x < 0.5 && j.y >= -0.5 && j.y < 0.5));
        let mean = js.iter().fold(Vec2::ZERO, |a, j| a + *j) / js.len() as f32;
        assert!(mean.length() < 0.07, "mean jitter {mean}");
        for q in 0..4 {
            let n = js.iter().filter(|j| (j.x >= 0.0) == (q & 1 == 1) && (j.y >= 0.0) == (q & 2 == 2)).count();
            assert!((2..=6).contains(&n), "quadrant {q}: {n} of 16");
        }
        assert_eq!(jitter_px(3), jitter_px(3 + JITTER_PHASES));
    }

    fn test_cam(eye: Vec3, size: (f32, f32)) -> Mat4 {
        let view = glam::camera::rh::view::look_to_mat4(eye, Vec3::NEG_Z, Vec3::Y);
        glam::camera::rh::proj::directx::perspective(60f32.to_radians(), size.0 / size.1, 0.1, 900.0) * view
    }

    /// project a world point to pixel coordinates (y down) and its depth-buffer value
    fn to_px(vp: &Mat4, w: Vec3, size: (f32, f32)) -> (Vec2, f32) {
        let c = *vp * w.extend(1.0);
        let n = c.truncate() / c.w;
        (Vec2::new((n.x * 0.5 + 0.5) * size.0, (0.5 - n.y * 0.5) * size.1), n.z)
    }

    #[test]
    fn jitter_shifts_the_image_not_the_depth() {
        let size = (640.0, 400.0);
        let vp = test_cam(Vec3::ZERO, size);
        let w = Vec3::new(3.0, 1.0, -20.0);
        let j = Vec2::new(0.25, -0.375);
        let (a, za) = to_px(&vp, w, size);
        let (b, zb) = to_px(&jittered(vp, j, size), w, size);
        assert!(((b - a) - j).length() < 1e-3, "image moved by {} not {j}", b - a);
        assert!((za - zb).abs() < 1e-7, "jitter must not change depth");
        // same result jittering the projection alone (three.js: after gl_to_metal) or the view-projection
        let view = glam::camera::rh::view::look_to_mat4(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y);
        let proj = glam::camera::rh::proj::directx::perspective(60f32.to_radians(), size.0 / size.1, 0.1, 900.0);
        assert!((jittered(proj, j, size) * view - jittered(proj * view, j, size)).abs_diff_eq(Mat4::ZERO, 1e-6));
    }

    #[test]
    fn reprojection_of_a_static_point_under_camera_translation() {
        let size = (640.0, 400.0);
        let world = Vec3::new(2.0, 1.5, -25.0);
        // previous frame: camera at the origin; this frame: 1 m to the right and 2 m forward
        let prev = test_cam(Vec3::ZERO, size);
        let cur = test_cam(Vec3::new(1.0, 0.0, -2.0), size);
        for j in [Vec2::ZERO, jitter_px(5), jitter_px(11)] {
            let cur_j = jittered(cur, j, size);
            // the pixel that rasterised the point this frame (jittered), and its depth value
            let (p, depth) = to_px(&cur_j, world, size);
            let px = (p.x.floor() as u32, p.y.floor() as u32);
            let m = reproject_motion(&cur_j.inverse(), &cur, &prev, px, depth, size);
            // pixel delta of the point itself, prev - cur, unjittered: within a pixel of the motion
            // read at the (pixel-centre-snapped) sample, with no trace of the jitter
            let (pp, _) = to_px(&prev, world, size);
            let (pc, _) = to_px(&cur, world, size);
            let px_motion = m * Vec2::new(size.0, size.1);
            assert!((px_motion - (pp - pc)).length() < 0.2, "jitter {j}: {px_motion} vs {}", pp - pc);
            // signs: the camera's 1 m move right dominates (the point sat further right last frame:
            // +x); the point is above centre and was further away, i.e. nearer the centre (+y, down)
            assert!(px_motion.x > 5.0 && px_motion.y > 0.0, "{px_motion}");
        }
        // closed form for a pure sideways move of t = 1 m at distance z = 25 m:
        // the image moves left by f_x * (w/2) * t / z, so motion (prev - cur) = +that
        let cur = test_cam(Vec3::new(1.0, 0.0, 0.0), size);
        let (p, depth) = to_px(&cur, Vec3::new(1.0, 0.0, -25.0), size); // screen centre
        let m = reproject_motion(&cur.inverse(), &cur, &prev, (p.x as u32, p.y as u32), depth, size) * Vec2::new(size.0, size.1);
        let fx = 1.0 / (30f32.to_radians().tan() * size.0 / size.1);
        let expected = fx * (size.0 * 0.5) / 25.0; // ~ 34.6 px
        assert!((m.x - expected).abs() < 0.05 && m.y.abs() < 0.05, "{m} vs ({expected}, 0)");
        // a still camera gives zero motion whatever the jitter
        let j = jittered(prev, jitter_px(7), size);
        let still = reproject_motion(&j.inverse(), &prev, &prev, (100, 300), 0.97, size);
        assert!(still.length() < 1e-5, "{still}");
        assert!(is_camera_cut(Vec3::ZERO, Vec3::new(0.0, 0.0, 50.0)) && !is_camera_cut(Vec3::ZERO, Vec3::new(0.0, 0.0, 0.2)));
    }

    #[test]
    fn args_parse() {
        let mut c = Config::default();
        let a: Vec<String> = "--path forward --scale 0.65 --size 1470x956 --lights 9999 --memoryless 0"
            .split(' ').map(String::from).collect();
        c.apply_args(&a).unwrap();
        assert_eq!(c.path, RenderPath::Forward);
        assert_eq!((c.width, c.height), (1470, 956));
        assert_eq!(c.lights, MAX_LIGHTS);
        assert!(!c.memoryless);
        assert!(c.apply_args(&["--bogus".into(), "1".into()]).is_err());
        c.apply_args(&["--upscale".into(), "temporal".into()]).unwrap();
        assert_eq!(c.upscale, Upscale::Temporal);
        assert!(c.label().contains("up=temporal"));
        assert_eq!(bench_matrix(&c).iter().filter(|(_, r)| r.upscale == Upscale::Temporal).count(), 2);
    }
}
