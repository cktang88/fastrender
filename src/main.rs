mod core;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
mod app;

const USAGE: &str = "fastrender [run|bench|matrix|probe] [--key value ...]
  run      window, live stats in the title bar, hot-reloads shaders/fastrender.metal (default)
  bench    offscreen benchmark of one config
  matrix   offscreen benchmark of the hypothesis matrix (A..L), writes bench-results.json
  probe    print GPU capabilities
  selftest stage-by-stage render diagnostics (writes selftest-*.png)
options: --path forward|deferred --memoryless 0|1 --scale 0.25..1 --upscale metalfx|bilinear
         --shadow-every 0(cached)|n --lights n --cull 0|1 --size WxH --props n --frames n
         --warmup n --vsync 0|1 --dynres <target ms> --log-every <s> --seed n";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mode, rest) = match args.first().map(String::as_str) {
        Some("run" | "bench" | "matrix" | "probe" | "selftest") => (args[0].clone(), &args[1..]),
        Some("-h" | "--help") => { println!("{USAGE}"); return; }
        _ => ("run".to_string(), &args[..]),
    };
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
            let mut out = Vec::new();
            for (name, c) in core::bench_matrix(&cfg) {
                eprintln!("running {name} ...");
                out.push(mac::bench(&c, &name));
            }
            mac::print_results(&mac::device_name(), &out);
        }
        _ => app::run(cfg),
    }
}

#[cfg(not(target_os = "macos"))]
fn run(_mode: &str, _cfg: core::Config) {
    eprintln!("fastrender's renderer is macOS (Metal) only; `cargo test` runs the portable core here.");
}
