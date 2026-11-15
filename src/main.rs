mod core;
mod voxel;
mod script;
mod three;
mod xlat;
mod image; // __fr.readFile / __fr.decodeImage for real web games (JavaScriptCore host)
#[cfg(target_os = "macos")]
mod jsc;
#[cfg(target_os = "macos")]
mod mac_three;
#[cfg(target_os = "macos")]
mod dfg_lut;
#[cfg(target_os = "macos")]
mod mac_programs;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
mod app;

const USAGE: &str = "fastrender [run|bench|matrix|probe] [--key value ...]
  run      window, live stats in the title bar, hot-reloads shaders/fastrender.metal (default)
  bench    offscreen benchmark of one config
  matrix   offscreen benchmark of the hypothesis matrix (A..L), writes bench-results.json
  probe    print GPU capabilities
  voxel-mesh  generate a 512x128x512 island and greedy-mesh it natively (any OS)
  selftest stage-by-stage render diagnostics (writes selftest-*.png)
  xlat-dump <dir> <out>  translate every NAME.vert.glsl/NAME.frag.glsl three.js program in <dir> to MSL (any OS)
options: --scene city|voxel|script|three --script game.js (props = box count) --three bundle.js (real three.js game) --programs three|native --path forward|deferred --memoryless 0|1 --scale 0.25..1 --upscale metalfx|temporal|bilinear
         --shadow-every 0(cached)|n --lights n --cull 0|1 --size WxH --props n --frames n
         --warmup n --seconds s --warmup-seconds s --rounds n --vsync 0|1 --dynres <target ms> --log-every <s> --seed n";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mode, rest) = match args.first().map(String::as_str) {
        Some("run" | "bench" | "matrix" | "probe" | "selftest" | "voxel-mesh" | "xlat-dump") => (args[0].clone(), &args[1..]),
        Some("-h" | "--help") => { println!("{USAGE}"); return; }
        _ => ("run".to_string(), &args[..]),
    };
    if mode == "xlat-dump" {
        let (Some(input), Some(out)) = (rest.first(), rest.get(1)) else {
            eprintln!("usage: fastrender xlat-dump <dir> <out>");
            std::process::exit(2);
        };
        if let Err(e) = xlat::dump(std::path::Path::new(input), std::path::Path::new(out)) {
            eprintln!("xlat-dump: {e}");
            std::process::exit(1);
        }
        return;
    }
    if mode == "voxel-mesh" {
        voxel_mesh_bench();
        return;
    }
    let mut cfg = core::Config::default();
    if let Err(e) = cfg.apply_args(rest) {
        eprintln!("{e}\n\n{USAGE}");
        std::process::exit(2);
    }
    run(&mode, cfg);
}

#[cfg(target_os = "macos")]
fn run(mode: &str, cfg: core::Config) {
    match mode {
        "probe" => mac::probe(),
        "selftest" => mac::selftest(cfg),
        "bench" => {
            let r = mac::bench(&cfg, &cfg.label());
            mac::print_results(&mac::device_name(), &[r]);
        }
        "matrix" => {
            let out = mac::matrix(&cfg);
            mac::print_results(&mac::device_name(), &out);
        }
        _ => app::run(cfg),
    }
}

#[cfg(not(target_os = "macos"))]
fn run(_mode: &str, _cfg: core::Config) {
    eprintln!("fastrender's renderer is macOS (Metal) only; `cargo test` runs the portable core here.");
}

fn voxel_mesh_bench() {
    let t = std::time::Instant::now();
    let world = voxel::generate(16, 4, 16, 7);
    let gen_ms = t.elapsed().as_secs_f64() * 1000.0;
    let m = voxel::mesh_world(&world);
    let tris = m.indices.len() / 3;
    println!("world {}x{}x{}: generate {:.0} ms", world.w, world.h, world.d, gen_ms);
    println!(
        "mesh {} chunks on {} threads: {:.0} ms total, {:.3} ms/chunk wall ({} non-empty), {} verts ({:.1} MB), {} tris",
        m.chunks_meshed, rayon::current_num_threads(), m.mesh_ms, m.mesh_ms / m.chunks_meshed as f64, m.chunks.len(),
        m.vertices.len(), (m.vertices.len() * 8 + m.indices.len() * 4) as f64 / 1e6, tris
    );
}
