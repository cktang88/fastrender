//! Where a long frame went (play mode's stall log, docs/HOST.md "Play mode"; pure, unit-tested on Linux).
//!
//! The render thread makes one [`FrameRec`] per frame it renders: the time since the frame before, how long it
//! waited for the page's frame, what it did itself (uploads applied, pipelines waited for, shadow and render-target
//! passes), and, when the GPU has completed the frame, its GPU ms. [`FrameLog`] keeps the distribution of every
//! frame and hands back the records of the long ones as one JSON line each (`host/electron/game/main.js` joins them
//! with the page's own record of that frame and writes the log).

use std::collections::VecDeque;

/// What a pipeline wait on the render thread was for (`mac_programs::Programs`).
pub const WAIT_MAIN: usize = 0;
pub const WAIT_SHADOW: usize = 1;
pub const WAIT_TARGET: usize = 2;
pub const WAIT_NAMES: [&str; 3] = ["main pass (replaces a pipeline on screen)", "shadow map", "render target"];

/// Cumulative counters of the three.js scene, read once per frame (`ThreeScene::frame_counters`); the render loop
/// takes differences.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Counters {
    /// render-thread waits for pipelines: count and ms, per `WAIT_*`
    pub waits: [u64; 3],
    pub wait_ms: [f64; 3],
    /// stage libraries compiled and pipelines built by the workers so far
    pub libraries: u64,
    pub pipelines: u64,
    /// draws skipped because their pipeline was still compiling
    pub skipped_draws: u64,
    /// archive / manifest writes finished (on a worker), and their ms
    pub persists: u64,
    pub persist_ms: f64,
}

/// One rendered frame.
#[derive(Clone, Debug, Default)]
pub struct FrameRec {
    /// ring sequence of the page's frame (what `present` returned to the page)
    pub seq: u64,
    /// unix ms when the frame was handed to the GPU
    pub at_ms: f64,
    /// since the frame before was handed to the GPU: what the player sees as the frame time
    pub interval_ms: f64,
    /// of that, waiting for the page to send the frame (the game's JS, or the page's own pacing)
    pub idle_ms: f64,
    /// render thread: the whole of `render()`, and inside it applying resources, waiting for a frame slot (GPU
    /// behind), waiting for the drawable (vsync / compositor)
    pub cpu_ms: f64,
    pub apply_ms: f64,
    pub fence_ms: f64,
    pub drawable_ms: f64,
    pub waits: [u32; 3],
    pub wait_ms: [f64; 3],
    /// resource messages applied with this frame, their payload bytes, programs added
    pub resources: u32,
    pub resource_bytes: u64,
    pub programs: u32,
    pub libraries: u32,
    pub pipelines: u32,
    /// programs + pipelines still compiling after the frame
    pub pending: u32,
    pub skipped_draws: u32,
    /// an archive / manifest write ended during this frame (ms on its worker), or one is under way
    pub persist_ms: f64,
    pub persisting: bool,
    pub draws: u32,
    pub shadow_passes: u32,
    pub shadow_draws: u32,
    /// passes into render targets (PMREM, post-processing, captures)
    pub target_passes: u32,
    /// frames of the page dropped in favour of this one
    pub skipped_frames: u32,
    pub gpu_ms: Option<f64>,
    /// targets, shadow, main, upscale (`{"gpu-passes": 1}`)
    pub pass_ms: Option<[f64; 4]>,
}

fn r1(v: f64) -> f64 { (v * 10.0).round() / 10.0 }

impl FrameRec {
    pub fn total_wait_ms(&self) -> f64 { self.wait_ms.iter().sum() }

    /// The largest share of the frame's time, in words. `over`: the threshold a part must reach to be named.
    pub fn cause(&self, over: f64) -> &'static str {
        let gpu = self.gpu_ms.unwrap_or(0.0);
        // (name, ms): the render thread's own parts first, then what it waited on
        let w = self.wait_ms;
        let rest = (self.cpu_ms - self.apply_ms - self.fence_ms - self.drawable_ms - self.total_wait_ms()).max(0.0);
        let parts: [(&'static str, f64); 9] = [
            ("pipeline wait: main pass", w[WAIT_MAIN]),
            ("pipeline wait: shadow map", w[WAIT_SHADOW]),
            ("pipeline wait: render target", w[WAIT_TARGET]),
            ("uploads", self.apply_ms),
            ("render thread (prepare / encode)", rest),
            ("GPU behind (frame slot)", self.fence_ms),
            ("GPU", gpu),
            ("drawable (vsync / compositor)", self.drawable_ms),
            ("page (JS / pacing)", self.idle_ms),
        ];
        let mut best = ("unknown", 0.0);
        for p in parts {
            if p.1 > best.1 {
                best = p;
            }
        }
        // a frame slot or drawable wait is the GPU's doing when the GPU frame itself was long
        if (best.0.starts_with("GPU behind") || best.0.starts_with("drawable")) && gpu >= over {
            return "GPU";
        }
        if best.1 < over * 0.5 { "unknown (no part over half the threshold)" } else { best.0 }
    }

    /// One JSON object, one line.
    pub fn json(&self, over: f64) -> String {
        let mut o = serde_json::Map::new();
        let mut put = |k: &str, v: serde_json::Value| {
            o.insert(k.to_string(), v);
        };
        put("seq", self.seq.into());
        put("at_ms", self.at_ms.round().into());
        put("interval_ms", r1(self.interval_ms).into());
        put("idle_ms", r1(self.idle_ms).into());
        put("cpu_ms", r1(self.cpu_ms).into());
        put("apply_ms", r1(self.apply_ms).into());
        put("fence_ms", r1(self.fence_ms).into());
        put("drawable_ms", r1(self.drawable_ms).into());
        put("waits", self.waits.to_vec().into());
        put("wait_ms", self.wait_ms.iter().map(|&v| r1(v)).collect::<Vec<_>>().into());
        put("resources", self.resources.into());
        put("resource_mb", ((self.resource_bytes as f64 / 1e6 * 100.0).round() / 100.0).into());
        put("programs", self.programs.into());
        put("libraries", self.libraries.into());
        put("pipelines", self.pipelines.into());
        put("pending", self.pending.into());
        put("skipped_draws", self.skipped_draws.into());
        put("persist_ms", r1(self.persist_ms).into());
        put("persisting", self.persisting.into());
        put("draws", self.draws.into());
        put("shadow_passes", self.shadow_passes.into());
        put("shadow_draws", self.shadow_draws.into());
        put("target_passes", self.target_passes.into());
        put("skipped_frames", self.skipped_frames.into());
        put("gpu_ms", self.gpu_ms.map_or(serde_json::Value::Null, |v| r1(v).into()));
        put("pass_ms", self.pass_ms.map_or(serde_json::Value::Null, |p| p.iter().map(|&v| r1(v)).collect::<Vec<_>>().into()));
        put("cause", self.cause(over).into());
        serde_json::Value::Object(o).to_string()
    }
}

/// p50 / p95 / p99 / max of a sample set (nearest rank); zeros when empty.
pub fn percentiles(v: &mut [f32]) -> [f64; 4] {
    if v.is_empty() {
        return [0.0; 4];
    }
    v.sort_unstable_by(|a, b| a.total_cmp(b));
    let at = |q: f64| v[(((v.len() as f64) * q).ceil() as usize).clamp(1, v.len()) - 1] as f64;
    [at(0.50), at(0.95), at(0.99), v[v.len() - 1] as f64]
}

/// Every frame's numbers, and the long frames' records.
pub struct FrameLog {
    /// a frame is long when its interval, its render-thread time or its GPU time is over this (ms)
    pub over: f64,
    /// frames handed to the GPU whose GPU time has not come back yet (they complete in order)
    waiting: VecDeque<FrameRec>,
    intervals: Vec<f32>,
    cpu: Vec<f32>,
    gpu: Vec<f32>,
    /// (cause, long frames, their interval ms in total, the longest)
    causes: Vec<(&'static str, u32, f64, f64)>,
    pub frames: u64,
    pub long: u64,
    gpu_n: u64,
    last: Counters,
    /// samples kept per series (a ring after that: an hour at 120 fps is 432,000)
    cap: usize,
}

impl FrameLog {
    pub fn new(over: f64) -> Self {
        FrameLog { over, waiting: VecDeque::new(), intervals: Vec::new(), cpu: Vec::new(), gpu: Vec::new(), causes: Vec::new(), frames: 0, long: 0, gpu_n: 0, last: Counters::default(), cap: 1 << 20 }
    }

    /// The counters' change since the last call, written into `rec`.
    pub fn diff(&mut self, now: Counters, rec: &mut FrameRec) {
        let l = self.last;
        for i in 0..3 {
            rec.waits[i] = now.waits[i].saturating_sub(l.waits[i]) as u32;
            rec.wait_ms[i] = (now.wait_ms[i] - l.wait_ms[i]).max(0.0);
        }
        rec.libraries = now.libraries.saturating_sub(l.libraries) as u32;
        rec.pipelines = now.pipelines.saturating_sub(l.pipelines) as u32;
        rec.skipped_draws = now.skipped_draws.saturating_sub(l.skipped_draws) as u32;
        rec.persist_ms = if now.persists > l.persists { (now.persist_ms - l.persist_ms).max(0.0) } else { 0.0 };
        self.last = now;
    }

    fn keep(v: &mut Vec<f32>, cap: usize, n: u64, x: f64) {
        if v.len() < cap { v.push(x as f32) } else { v[(n % cap as u64) as usize] = x as f32 }
    }

    /// A frame was handed to the GPU. The first frame's interval (since launch) is not a frame time: pass 0.
    pub fn submitted(&mut self, rec: FrameRec, out: &mut Vec<String>) {
        self.frames += 1;
        if rec.interval_ms > 0.0 {
            Self::keep(&mut self.intervals, self.cap, self.frames, rec.interval_ms);
        }
        Self::keep(&mut self.cpu, self.cap, self.frames, rec.cpu_ms);
        self.waiting.push_back(rec);
        // a frame whose GPU time never comes (an error, a frame without a command buffer) must not hold the others
        while self.waiting.len() > 6 {
            let r = self.waiting.pop_front().unwrap();
            self.finish(r, out);
        }
    }

    /// The oldest waiting frame completed on the GPU.
    pub fn completed(&mut self, gpu_ms: f64, pass_ms: Option<[f64; 4]>, out: &mut Vec<String>) {
        self.gpu_n += 1;
        Self::keep(&mut self.gpu, self.cap, self.gpu_n, gpu_ms);
        if let Some(mut r) = self.waiting.pop_front() {
            r.gpu_ms = Some(gpu_ms);
            r.pass_ms = pass_ms;
            self.finish(r, out);
        }
    }

    /// At shutdown: the frames still waiting for their GPU time.
    pub fn flush(&mut self, out: &mut Vec<String>) {
        while let Some(r) = self.waiting.pop_front() {
            self.finish(r, out);
        }
    }

    fn finish(&mut self, r: FrameRec, out: &mut Vec<String>) {
        let long = r.interval_ms > self.over || r.cpu_ms > self.over || r.gpu_ms.is_some_and(|g| g > self.over);
        if !long {
            return;
        }
        self.long += 1;
        let cause = r.cause(self.over);
        let ms = r.interval_ms.max(r.cpu_ms);
        match self.causes.iter_mut().find(|c| c.0 == cause) {
            Some(c) => {
                c.1 += 1;
                c.2 += ms;
                c.3 = c.3.max(ms);
            }
            None => self.causes.push((cause, 1, ms, ms)),
        }
        if out.len() < 4096 {
            out.push(r.json(self.over));
        }
    }

    /// `{"frames", "long", "over_ms", "interval_ms": [p50, p95, p99, max], "cpu_ms", "gpu_ms", "causes": [{cause, frames,
    /// ms, max_ms}]}` with the causes by total ms, largest first.
    pub fn summary_json(&mut self) -> String {
        let q = |v: &mut Vec<f32>| serde_json::Value::from(percentiles(v).iter().map(|&x| r1(x)).collect::<Vec<_>>());
        let mut causes = self.causes.clone();
        causes.sort_by(|a, b| b.2.total_cmp(&a.2));
        serde_json::json!({
            "frames": self.frames, "long": self.long, "over_ms": self.over,
            "interval_ms": q(&mut self.intervals), "cpu_ms": q(&mut self.cpu), "gpu_ms": q(&mut self.gpu),
            "causes": causes.iter().map(|c| serde_json::json!({ "cause": c.0, "frames": c.1, "ms": r1(c.2), "max_ms": r1(c.3) })).collect::<Vec<_>>(),
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(interval: f64) -> FrameRec { FrameRec { interval_ms: interval, cpu_ms: 3.0, ..Default::default() } }

    #[test]
    fn the_cause_is_the_largest_part() {
        let mut r = rec(7200.0);
        r.cpu_ms = 7150.0;
        r.wait_ms = [7100.0, 0.0, 0.0];
        r.waits = [61, 0, 0];
        assert_eq!(r.cause(25.0), "pipeline wait: main pass");
        let mut r = rec(48.0);
        r.idle_ms = 44.0;
        assert_eq!(r.cause(25.0), "page (JS / pacing)");
        // the render thread stood in nextDrawable because the GPU frame was long
        let mut r = rec(50.0);
        r.cpu_ms = 45.0;
        r.drawable_ms = 40.0;
        r.gpu_ms = Some(42.0);
        assert_eq!(r.cause(25.0), "GPU");
        let mut r = rec(60.0);
        r.cpu_ms = 55.0;
        r.apply_ms = 50.0;
        assert_eq!(r.cause(25.0), "uploads");
        let mut r = rec(60.0);
        r.cpu_ms = 55.0;
        assert_eq!(r.cause(25.0), "render thread (prepare / encode)");
        assert!(rec(26.0).cause(25.0).starts_with("unknown"));
    }

    #[test]
    fn long_frames_come_out_with_their_gpu_time_and_the_rest_only_counts() {
        let mut log = FrameLog::new(25.0);
        let mut out = Vec::new();
        for i in 0..100u64 {
            let mut r = rec(if i == 40 { 300.0 } else { 16.7 });
            r.seq = i;
            if i == 40 {
                r.idle_ms = 280.0;
            }
            log.submitted(r, &mut out);
            // the GPU completes a frame behind
            if i > 0 {
                log.completed(if i == 71 { 31.0 } else { 7.5 }, Some([0.1, 1.0, 6.4, 0.0]), &mut out);
            }
        }
        log.flush(&mut out);
        assert_eq!(out.len(), 2, "{out:?}");
        let a: serde_json::Value = serde_json::from_str(&out[0]).unwrap();
        assert_eq!((a["seq"].as_u64(), a["interval_ms"].as_f64(), a["gpu_ms"].as_f64(), a["cause"].as_str()), (Some(40), Some(300.0), Some(7.5), Some("page (JS / pacing)")));
        assert_eq!(a["pass_ms"][2].as_f64(), Some(6.4));
        // frame 70's GPU time arrived with frame 71's submit
        let b: serde_json::Value = serde_json::from_str(&out[1]).unwrap();
        assert_eq!((b["seq"].as_u64(), b["gpu_ms"].as_f64(), b["cause"].as_str()), (Some(70), Some(31.0), Some("GPU")));
        let s: serde_json::Value = serde_json::from_str(&log.summary_json()).unwrap();
        assert_eq!((s["frames"].as_u64(), s["long"].as_u64()), (Some(100), Some(2)));
        assert_eq!(s["interval_ms"][0].as_f64(), Some(16.7));
        assert_eq!(s["interval_ms"][3].as_f64(), Some(300.0));
        assert_eq!(s["causes"][0]["cause"].as_str(), Some("page (JS / pacing)"));
    }

    #[test]
    fn counters_become_per_frame_numbers() {
        let mut log = FrameLog::new(25.0);
        let mut r = FrameRec::default();
        log.diff(Counters { waits: [2, 0, 1], wait_ms: [40.0, 0.0, 9.0], libraries: 10, pipelines: 12, skipped_draws: 3, persists: 0, persist_ms: 0.0 }, &mut r);
        assert_eq!((r.waits, r.libraries, r.pipelines, r.skipped_draws), ([2, 0, 1], 10, 12, 3));
        log.diff(Counters { waits: [2, 4, 1], wait_ms: [40.0, 900.0, 9.0], libraries: 14, pipelines: 12, skipped_draws: 3, persists: 1, persist_ms: 1500.0 }, &mut r);
        assert_eq!((r.waits, r.wait_ms, r.libraries, r.pipelines, r.skipped_draws, r.persist_ms), ([0, 4, 0], [0.0, 900.0, 0.0], 4, 0, 0, 1500.0));
        assert_eq!(r.cause(25.0), "pipeline wait: shadow map");
    }

    #[test]
    fn percentiles_by_nearest_rank() {
        let mut v: Vec<f32> = (1..=100).map(|x| x as f32).collect();
        assert_eq!(percentiles(&mut v), [50.0, 95.0, 99.0, 100.0]);
        assert_eq!(percentiles(&mut []), [0.0; 4]);
        assert_eq!(percentiles(&mut [7.0]), [7.0; 4]);
    }
}
