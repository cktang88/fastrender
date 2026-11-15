//! Platform-independent parts: config, procedural test city, camera path, culling math,
//! frame statistics and the dynamic-resolution controller. Unit-tested on any OS.

use glam::{Mat4, Vec3, Vec4};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderPath {
    /// one pass: geometry shaded directly (sun + shadow + brute-force point-light loop)
    Forward,
    /// one render pass: G-buffer in tile memory, read back with programmable blending,
    /// light volumes and tonemap in the same pass (Apple TBDR "single-pass deferred")
    Deferred,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Upscale {
    /// bilinear full-screen pass from the internal resolution to the output
    Bilinear,
    /// MetalFX spatial scaler
    Spatial,
}

#[derive(Clone, Debug)]
pub struct Config {
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
    pub vsync: bool,
    /// dynamic resolution target in ms (0 = off), windowed mode only
    pub dynres_ms: f32,
    /// print fps every n seconds of the run (bench mode, for thermal-throttling runs)
    pub log_every_s: f32,
    pub seed: u64,
    /// bench mode: save the final frame as PNG
    pub shot: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
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
            vsync: true,
            dynres_ms: 0.0,
            log_every_s: 0.0,
            seed: 20261115,
            shot: None,
        }
    }
}

impl Config {
    pub fn label(&self) -> String {
        format!(
            "{} {} scale={:.2} up={} shadow={} lights={} cull={} props={}",
            match self.path { RenderPath::Forward => "forward", RenderPath::Deferred => "deferred" },
            if self.memoryless { "memoryless" } else { "stored" },
            self.scale,
            match self.upscale { Upscale::Bilinear => "bilinear", Upscale::Spatial => "metalfx" },
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
                "path" => self.path = match v.as_str() { "forward" => RenderPath::Forward, "deferred" => RenderPath::Deferred, _ => return Err(format!("bad --path {v}")) },
                "memoryless" => self.memoryless = num(v)? != 0.0,
                "scale" => self.scale = (num(v)? as f32).clamp(0.25, 1.0),
                "upscale" => self.upscale = match v.as_str() { "bilinear" | "none" => Upscale::Bilinear, "metalfx" | "spatial" => Upscale::Spatial, _ => return Err(format!("bad --upscale {v}")) },
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
                "vsync" => self.vsync = num(v)? != 0.0,
                "dynres" => self.dynres_ms = num(v)? as f32,
                "log-every" => self.log_every_s = num(v)? as f32,
                "seed" => self.seed = num(v)? as u64,
                "shot" => self.shot = Some(v.clone()),
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
    // street lights at intersections, then mid-block
    let mut ls = Vec::new();
    let mut i = 0u32;
    'outer: for pass in 0..4 {
        for bx in -BLOCKS / 2..=BLOCKS / 2 {
            for bz in -BLOCKS / 2..=BLOCKS / 2 {
                if ls.len() as u32 >= lights { break 'outer; }
                let x = bx as f32 * pitch_x - STREET * 0.5 + (pass % 2) as f32 * pitch_x * 0.5;
                let z = bz as f32 * pitch_z - STREET * 0.5 + (pass / 2) as f32 * pitch_z * 0.5;
                let hue = (i % 3) as f32;
                ls.push(Light { pos_radius: [x, 5.0, z, 18.0], color: [3.0 + hue * 0.2, 2.2, 1.2 + hue * 0.3, 0.0] });
                i += 1;
            }
        }
    }
    Scene { instances: inst, lights: ls, extent }
}

/// Deterministic street-level camera path: down an avenue, turn, across a street.
pub fn camera_at(t: f32) -> (Vec3, Vec3) {
    let pitch_x = BLOCK_X + STREET;
    let speed = 9.0; // m/s, a slow drive
    let len = BLOCKS as f32 * pitch_x * 0.8;
    let s = (t * speed) % (2.0 * len);
    let avenue_x = -STREET * 0.5 + pitch_x; // centre of a street between blocks
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
    v.push(("L no GPU culling (vs G)".into(), mk(&|c| { c.path = Deferred; c.scale = 0.5; c.upscale = Spatial; c.lights = 0; c.gpu_cull = false; })));
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
    }

    #[test]
    fn camera_stays_in_city() {
        let s = build_city(0, 0, 1);
        for i in 0..2000 {
            let (p, d) = camera_at(i as f32 * 0.1);
            assert!(p.x.abs() < s.extent && p.z.abs() < s.extent, "camera left the city at {p}");
            assert!((d.length() - 1.0).abs() < 1e-4);
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
    }
}
