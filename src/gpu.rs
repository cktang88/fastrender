//! The backend seam (docs/notes/metal4-port.md, perf audit wave 2, package M-0).
//!
//! Metal 4 (`MTL4*`) is the production path; a thin Metal 3 path stays because GitHub's hosted runners (paravirtual
//! GPU) cannot execute Metal 4 and the goldens run there. A Metal 4 pipeline state, encoder or MetalFX scaler cannot be
//! mixed with Metal 3 command buffers, so the backend is chosen once per run ([`select`]) and everything that encodes
//! goes through two small traits:
//!
//! - [`Frame`]: one frame's command stream: render passes from a [`PassDesc`], a compute dispatch, texture copies, mip
//!   generation, the per-group GPU timing hook (`end_group`);
//! - [`RenderPass`]: an open render pass: pipeline / depth / raster state, buffers by (buffer, offset, index),
//!   textures and samplers by (stage, index) or from a bind list (src/bindlist.rs), draws.
//!
//! Call sites are generic over the trait (monomorphised per backend: no dynamic dispatch per draw). Implementations:
//! src/gpu_metal3.rs (today's encoder, moved behind the trait) and src/gpu_metal4.rs.
//!
//! This file also holds what is true on any OS and unit-tested on Linux: the backend choice and the **graveyard**,
//! the frame-indexed deferred-release queue. Metal 4 command buffers retain nothing, so a buffer / texture that is
//! replaced or released while frames are in flight must outlive every frame that can reference it. The renderer no
//! longer relies on Metal 3's retention either: every replace / release goes through [`Graveyard::bury`], and an item
//! is freed only once the GPU has completed the frame it was buried in (the WP5 counter fence, src/mac_frame.rs).

use std::collections::VecDeque;

// ------------------------------------------------------------------------------------------------- backend choice

/// The graphics API a run encodes with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Backend {
    Metal4,
    Metal3,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::Metal4 => "metal4",
            Backend::Metal3 => "metal3",
        }
    }
}

/// `--backend metal4|metal3|auto` (default auto = Metal 3 for now, see `select`), or `FASTRENDER_BACKEND` when the option was not given.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BackendChoice {
    #[default]
    Auto,
    Metal4,
    Metal3,
}

impl BackendChoice {
    pub fn parse(s: &str) -> Result<BackendChoice, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Ok(BackendChoice::Auto),
            "metal4" | "mtl4" | "4" => Ok(BackendChoice::Metal4),
            "metal3" | "mtl3" | "3" => Ok(BackendChoice::Metal3),
            _ => Err(format!("bad backend {s} (metal4|metal3|auto)")),
        }
    }

    /// The choice in effect: the command line when it named a backend, else `env` (the value of `FASTRENDER_BACKEND`),
    /// else auto. An unreadable env value is reported and ignored.
    pub fn resolve(self, env: Option<&str>) -> (BackendChoice, Option<String>) {
        if self != BackendChoice::Auto {
            return (self, None);
        }
        match env.map(BackendChoice::parse) {
            Some(Ok(c)) => (c, None),
            Some(Err(e)) => (BackendChoice::Auto, Some(format!("FASTRENDER_BACKEND ignored: {e}"))),
            None => (BackendChoice::Auto, None),
        }
    }
}

/// What the device said about Metal 4 (`metal::detect` on macOS; `fastrender probe` prints the same facts).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Metal4Support {
    /// `supportsFamily(MTLGPUFamilyMetal4)`
    pub family: bool,
    /// `newMTL4CommandQueue` returned a queue (only tried when the family is there: without it the call may raise)
    pub queue: bool,
}

impl Metal4Support {
    pub fn usable(self) -> bool { self.family && self.queue }

    fn why_not(self) -> &'static str {
        if !self.family { "the device does not report the Metal4 GPU family" } else { "MTL4CommandQueue creation failed" }
    }
}

/// The backend of this run, and a line for the log when the request could not be met (Metal 4 asked for on a
/// device without it: Metal 3 runs instead, the run does not fail).
pub fn select(choice: BackendChoice, dev: Metal4Support) -> (Backend, Option<String>) {
    match choice {
        BackendChoice::Metal3 => (Backend::Metal3, None),
        // Auto is Metal 3 for now: the Metal 4 backend has never run on hardware (src/gpu_metal4.rs), so it is opt-in
        // (`--backend metal4`). Once the Metal 4 lane is green this becomes `if dev.usable() { Metal4 } else { Metal3 }`.
        BackendChoice::Auto => (Backend::Metal3, None),
        BackendChoice::Metal4 if dev.usable() => (Backend::Metal4, None),
        BackendChoice::Metal4 => (Backend::Metal3, Some(format!("backend metal4 requested but {}: falling back to metal3", dev.why_not()))),
    }
}

// ------------------------------------------------------------------------------------------------------ graveyard

/// Frame ordinals of the frames-in-flight fence (src/mac_frame.rs), as the code that buries resources sees them.
/// Frames are 1-based; 0 = none.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FrameClock {
    /// the frame being encoded now (the next to be committed): the last frame that can reference a resource
    /// replaced or released now
    pub encoding: u64,
    /// every frame up to this one has completed on the GPU
    pub completed: u64,
}

/// Deferred release of GPU objects, indexed by frame.
///
/// `bury(frame, item)`: `item` was just taken out of use (replaced by a new buffer, released by the game, a render
/// target resized...) while `frame` was being encoded. Frames up to and including `frame` may have it bound in
/// commands the GPU has not run yet, so it must stay alive. `collect(completed)` frees everything buried in a
/// frame the GPU has completed. Nothing is ever freed earlier, whatever the backend retains by itself.
///
/// Items are kept in burial order; frames never go backwards (a smaller frame is treated as the newest seen), so
/// `collect` pops from the front and stops at the first item that must stay.
pub struct Graveyard<T> {
    q: VecDeque<(u64, T)>,
    newest: u64,
    /// totals, for the stats line and the tests
    pub buried: u64,
    pub freed: u64,
    pub peak: usize,
}

impl<T> Default for Graveyard<T> {
    fn default() -> Self { Self::new() }
}

impl<T> Graveyard<T> {
    pub fn new() -> Self { Graveyard { q: VecDeque::new(), newest: 0, buried: 0, freed: 0, peak: 0 } }

    /// Keep `item` alive until frame `frame` has completed.
    pub fn bury(&mut self, frame: u64, item: T) {
        self.newest = self.newest.max(frame);
        self.q.push_back((self.newest, item));
        self.buried += 1;
        self.peak = self.peak.max(self.q.len());
    }

    /// Free what was buried in frames `<= completed`, oldest first, through `free` (dropping the item, and whatever
    /// else the backend must do first: a Metal 4 residency set forgets the allocation there). Returns the count.
    pub fn collect_with(&mut self, completed: u64, mut free: impl FnMut(T)) -> usize {
        let mut n = 0;
        while self.q.front().is_some_and(|(f, _)| *f <= completed) {
            let (_, item) = self.q.pop_front().expect("front");
            free(item);
            n += 1;
        }
        self.freed += n as u64;
        n
    }

    /// `collect_with` that just drops the items.
    pub fn collect(&mut self, completed: u64) -> usize { self.collect_with(completed, drop) }

    /// Free everything: only when the GPU is idle (every committed frame completed and none being encoded).
    pub fn drain_idle(&mut self) -> usize { self.collect(u64::MAX) }

    /// Items waiting for their frame.
    pub fn len(&self) -> usize { self.q.len() }

    pub fn is_empty(&self) -> bool { self.q.is_empty() }

    /// The frame the oldest waiting item needs completed (None: empty).
    pub fn oldest(&self) -> Option<u64> { self.q.front().map(|(f, _)| *f) }
}

#[cfg(target_os = "macos")]
#[path = "gpu_encoder.rs"]
mod encoder;
#[cfg(target_os = "macos")]
pub use encoder::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn choice_parses_and_env_only_fills_auto() {
        assert_eq!(BackendChoice::parse("metal4"), Ok(BackendChoice::Metal4));
        assert_eq!(BackendChoice::parse("Metal3"), Ok(BackendChoice::Metal3));
        assert_eq!(BackendChoice::parse("auto"), Ok(BackendChoice::Auto));
        assert!(BackendChoice::parse("vulkan").is_err());
        // the command line wins over the environment
        assert_eq!(BackendChoice::Metal3.resolve(Some("metal4")).0, BackendChoice::Metal3);
        assert_eq!(BackendChoice::Auto.resolve(Some("metal4")).0, BackendChoice::Metal4);
        assert_eq!(BackendChoice::Auto.resolve(None).0, BackendChoice::Auto);
        let (c, note) = BackendChoice::Auto.resolve(Some("nope"));
        assert_eq!(c, BackendChoice::Auto);
        assert!(note.unwrap().contains("FASTRENDER_BACKEND"));
    }

    #[test]
    fn backend_follows_capability() {
        let (real, hosted, half) = (Metal4Support { family: true, queue: true }, Metal4Support::default(), Metal4Support { family: true, queue: false });
        // auto: Metal 3 on every device until the Metal 4 backend has been verified on hardware
        assert_eq!(select(BackendChoice::Auto, real), (Backend::Metal3, None));
        assert_eq!(select(BackendChoice::Auto, hosted), (Backend::Metal3, None));
        assert_eq!(select(BackendChoice::Auto, half), (Backend::Metal3, None));
        // forced Metal 3 never looks at the device
        assert_eq!(select(BackendChoice::Metal3, real), (Backend::Metal3, None));
        // forced Metal 4 on a device without it: Metal 3, with the reason (CI's paravirtual GPU)
        let (b, note) = select(BackendChoice::Metal4, hosted);
        assert_eq!(b, Backend::Metal3);
        assert!(note.unwrap().contains("Metal4 GPU family"));
        let (b, note) = select(BackendChoice::Metal4, half);
        assert_eq!(b, Backend::Metal3);
        assert!(note.unwrap().contains("MTL4CommandQueue"));
        assert_eq!(select(BackendChoice::Metal4, real), (Backend::Metal4, None));
    }

    /// A stand-in for a GPU object: counts its drop.
    struct Res(Rc<Cell<u32>>);
    impl Drop for Res {
        fn drop(&mut self) { self.0.set(self.0.get() + 1); }
    }

    #[test]
    fn nothing_is_freed_before_its_frame_completed() {
        let dropped = Rc::new(Cell::new(0));
        let mut g = Graveyard::new();
        // frame 5 is being encoded, frames 3 and 4 are in flight: a buffer is replaced
        g.bury(5, Res(dropped.clone()));
        assert_eq!((g.collect(3), g.collect(4), dropped.get()), (0, 0, 0), "frames in flight may still read it");
        assert_eq!((g.collect(5), dropped.get()), (1, 1));
        assert!(g.is_empty());
    }

    #[test]
    fn two_frames_in_flight_keep_both_generations() {
        // the case one batch of delay (three::defer_releases) does not cover: a buffer replaced in two consecutive
        // frames while the GPU is two frames behind
        let (a, b) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let mut g = Graveyard::new();
        g.bury(10, Res(a.clone())); // bound by frames <= 10
        g.bury(11, Res(b.clone())); // bound by frame 11 (and maybe 10)
        assert_eq!(g.collect(9), 0);
        assert_eq!((g.collect(10), a.get(), b.get()), (1, 1, 0));
        assert_eq!(g.oldest(), Some(11));
        assert_eq!((g.collect(11), b.get()), (1, 1));
    }

    #[test]
    fn collect_is_ordered_and_counts() {
        let d = Rc::new(Cell::new(0));
        let mut g = Graveyard::new();
        for f in [1u64, 1, 2, 2, 2, 3] {
            g.bury(f, Res(d.clone()));
        }
        assert_eq!((g.len(), g.peak, g.buried), (6, 6, 6));
        let mut order = Vec::new();
        assert_eq!(g.collect_with(2, |r| order.push(Rc::as_ptr(&r.0))), 5);
        assert_eq!((order.len(), d.get(), g.len(), g.freed), (5, 5, 1, 5));
        // completed never passing the frame keeps the item; an idle GPU frees everything
        assert_eq!(g.collect(2), 0);
        assert_eq!((g.drain_idle(), d.get(), g.freed), (1, 6, 6));
    }

    #[test]
    fn a_frame_that_goes_backwards_waits_for_the_newest() {
        // burying with a stale frame number must not free early, and must not block the queue behind it
        let d = Rc::new(Cell::new(0));
        let mut g = Graveyard::new();
        g.bury(7, Res(d.clone()));
        g.bury(4, Res(d.clone()));
        assert_eq!(g.collect(6), 0);
        assert_eq!((g.collect(7), d.get()), (2, 2));
    }

    #[test]
    fn steady_churn_stays_bounded() {
        // a buffer replaced every frame with the GPU two frames behind: never more than three generations wait
        let d = Rc::new(Cell::new(0));
        let mut g = Graveyard::new();
        for frame in 1..=1000u64 {
            g.collect(frame.saturating_sub(3)); // completed when frame starts
            g.bury(frame, Res(d.clone()));
            assert!(g.len() <= 3, "frame {frame}: {} waiting", g.len());
        }
        assert_eq!(d.get() as u64 + g.len() as u64, 1000);
    }

    #[test]
    fn dropping_the_graveyard_frees_the_rest() {
        let d = Rc::new(Cell::new(0));
        let mut g = Graveyard::new();
        g.bury(1, Res(d.clone()));
        g.bury(2, Res(d.clone()));
        drop(g);
        assert_eq!(d.get(), 2);
    }
}
