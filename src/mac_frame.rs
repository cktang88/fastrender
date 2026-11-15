//! Frame submission of `Renderer` (src/mac.rs, whose child module this is): the frames-in-flight fence, the frame's
//! command buffers with per-pass GPU timing, and the power state (thermal, Low Power Mode).
//!
//! **Pipelining.** The render thread encodes frame N+1 while the GPU runs N. `FrameFence` bounds how far ahead it
//! may get (2 windowed and in Electron, 3 in the offscreen bench): `begin` blocks only when that many frames are
//! already committed and unfinished, i.e. when the GPU is the bottleneck. Nothing else on the per-frame path waits
//! for the GPU (`waitUntilCompleted` exists only in readback / selftest code).
//!
//! The fence is a monotonic counter, not a per-frame semaphore: frame n (1-based) may start once
//! `completed >= n - limit`, and idle means `completed == submitted`. Two things can advance `completed`:
//! - the command buffer's completion handler (`Fence::Cpu`, the default);
//! - the GPU itself, through an `MTLSharedEvent` signalled by a command at the end of the frame (`Fence::Event`).
//!   This is the shape the Metal 4 port needs: `MTL4CommandQueue` has no completion handler per command buffer in
//!   the hot path, a frame ends with `queue.signalEvent(event, n)` and the CPU waits with
//!   `event.waitUntilSignaledValue(n - limit)` (docs/research/pocketcat-techniques.md PC5). The completion counter
//!   is kept as a safety net (a failed command buffer never signals), so a wait can never hang.
//!
//! **Per-pass GPU time** (PC1). `FrameCmds` is the seam: the frame calls `end_group(PASS_*)` after each pass group
//! (targets, shadow, main, upscale). With timing off that is a no-op and the frame is one command buffer. With
//! timing on, each group is committed as its own command buffer and its `GPUStartTime..GPUEndTime` is read in the
//! completion handler; groups run back to back on one queue, so a group's time is `end - max(start, previous end)`.
//! This works on every device (CI's paravirtual GPU has no counter sets) and for passes this file does not encode
//! (MetalFX, three.js's render targets). The same seam maps onto the other two timestamp sources:
//! - Metal 3 `MTLCounterSampleBuffer` (timestamp counter set, `supportsCounterSampling(.atStageBoundary)` on Apple
//!   GPUs): `end_group` becomes "the sample indices of this group's render-pass descriptors end here", resolved
//!   when the frame's slot is free again. It also splits vertex from fragment time, which command buffers cannot;
//! - Metal 4 `MTL4CounterHeap`: `end_group` becomes `commandBuffer.writeTimestamp(into: heap, at: slot * 8 + group)`
//!   and the heap range of a frame slot is resolved when the fence passes that frame.
//!
//! `fastrender probe` prints which of these the device has.

use super::*;
use crate::core::{Fence, LoopStats, PowerState, Thermal};

type SharedEvent = Retained<ProtocolObject<dyn MTLSharedEvent>>;

/// Completed-frame counter the completion handlers advance.
struct Completed {
    n: Mutex<u64>,
    cv: Condvar,
}

/// Frames-in-flight back-pressure (see the module docs).
pub(super) struct FrameFence {
    limit: u64,
    /// frames committed so far (render thread only)
    submitted: u64,
    done: Arc<Completed>,
    /// `Fence::Event`: signalled by the GPU with the frame's ordinal
    event: Option<SharedEvent>,
    /// waits on the event that timed out although the completion counter had passed (the event did not fire)
    pub(super) event_misses: std::cell::Cell<u64>,
}

impl FrameFence {
    pub(super) fn new(dev: &Device, limit: usize, kind: Fence) -> FrameFence {
        let event = if kind == Fence::Event { dev.newSharedEvent() } else { None };
        FrameFence { limit: limit as u64, submitted: 0, done: Arc::new(Completed { n: Mutex::new(0), cv: Condvar::new() }), event, event_misses: std::cell::Cell::new(0) }
    }

    pub(super) fn limit(&self) -> usize { self.limit as usize }

    pub(super) fn uses_event(&self) -> bool { self.event.is_some() }

    fn completed(&self) -> u64 {
        let cpu = *self.done.n.lock().unwrap();
        match &self.event {
            Some(e) => cpu.max(e.signaledValue()),
            None => cpu,
        }
    }

    /// Block until `value` frames have completed.
    fn wait_for(&self, value: u64) {
        if let Some(e) = &self.event {
            // the GPU's signal; re-checked against the completion counter so a lost signal costs 100 ms, not a hang
            while !e.waitUntilSignaledValue_timeoutMS(value, 100) {
                if *self.done.n.lock().unwrap() >= value {
                    self.event_misses.set(self.event_misses.get() + 1);
                    break;
                }
            }
            return;
        }
        let mut n = self.done.n.lock().unwrap();
        while *n < value {
            n = self.done.cv.wait(n).unwrap();
        }
    }

    /// Wait for a free frame slot. Returns the ms spent blocked (0 unless the GPU is behind).
    pub(super) fn begin(&mut self) -> f64 {
        let need = (self.submitted + 1).saturating_sub(self.limit);
        if self.completed() >= need {
            return 0.0;
        }
        let t = Instant::now();
        self.wait_for(need);
        t.elapsed().as_secs_f64() * 1000.0
    }

    /// The frame being encoded is about to be committed: encode the GPU-side signal into its last command buffer
    /// and count it. Returns (the frame's ordinal, frames in flight including this one, the completion handle).
    pub(super) fn submit(&mut self, last_cb: &CmdBuf) -> (u64, u32, FenceDone) {
        self.submitted += 1;
        if let Some(e) = &self.event {
            last_cb.encodeSignalEvent_value(ProtocolObject::from_ref(&**e), self.submitted);
        }
        let in_flight = (self.submitted - self.completed().min(self.submitted)) as u32;
        (self.submitted, in_flight, FenceDone { done: self.done.clone(), event: self.event.clone(), ordinal: self.submitted })
    }

    pub(super) fn wait_idle(&self) { self.wait_for(self.submitted); }
}

/// Moved into the last command buffer's completion handler.
pub(super) struct FenceDone {
    done: Arc<Completed>,
    event: Option<SharedEvent>,
    ordinal: u64,
}

impl FenceDone {
    /// `ok`: the command buffer completed without error (then the GPU has already signalled the event).
    pub(super) fn signal(&self, ok: bool) {
        if !ok {
            if let Some(e) = &self.event {
                // a failed command buffer does not run its signal command
                if e.signaledValue() < self.ordinal {
                    e.setSignaledValue(self.ordinal);
                }
            }
        }
        let mut n = self.done.n.lock().unwrap();
        *n = (*n).max(self.ordinal);
        self.done.cv.notify_all();
    }
}

/// GPU spans of one frame's pass groups, filled by the completion handlers.
#[derive(Default)]
pub(super) struct Spans(Vec<(usize, f64, f64)>);

impl Spans {
    pub(super) fn push(&mut self, group: usize, start: f64, end: f64) { self.0.push((group, start, end)); }

    /// Per-group ms: groups run back to back, so each is charged from when it could start (`prev_end`: the end of
    /// whatever ran before it) to its own end. Returns the times and the last end.
    pub(super) fn resolve(&self, mut prev_end: f64) -> ([f64; 4], f64) {
        let mut ms = [0.0; 4];
        for &(g, start, end) in &self.0 {
            if end <= 0.0 {
                continue; // an empty command buffer never ran on the GPU
            }
            ms[g] += (end - start.max(prev_end)).max(0.0) * 1000.0;
            prev_end = prev_end.max(end);
        }
        (ms, prev_end)
    }
}

/// The command buffer(s) of one frame (see the module docs).
pub(super) struct FrameCmds {
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    cb: Retained<CmdBuf>,
    /// Some: per-pass timing, one command buffer per pass group
    pub(super) spans: Option<Arc<Mutex<Spans>>>,
}

impl FrameCmds {
    pub(super) fn new(queue: &Retained<ProtocolObject<dyn MTLCommandQueue>>, timed: bool) -> FrameCmds {
        FrameCmds { queue: queue.clone(), cb: queue.commandBuffer().expect("command buffer"), spans: timed.then(|| Arc::new(Mutex::new(Spans::default()))) }
    }

    /// The command buffer to encode into.
    pub(super) fn cb(&self) -> &CmdBuf { &self.cb }

    /// Everything encoded since the last call belongs to pass group `group`.
    pub(super) fn end_group(&mut self, group: usize) {
        let Some(spans) = &self.spans else { return };
        let spans = spans.clone();
        let handler = RcBlock::new(move |cb: NonNull<CmdBuf>| {
            let cb = unsafe { cb.as_ref() };
            spans.lock().unwrap().push(group, cb.GPUStartTime(), cb.GPUEndTime());
        });
        unsafe { self.cb.addCompletedHandler(RcBlock::as_ptr(&handler)) };
        self.cb.commit();
        self.cb = self.queue.commandBuffer().expect("command buffer");
    }

    /// The last command buffer (present + the frame's completion handler go on it), tagged as `group`.
    pub(super) fn finish(self, group: usize) -> (Retained<CmdBuf>, Option<(Arc<Mutex<Spans>>, usize)>) { (self.cb, self.spans.map(|s| (s, group))) }
}

/// Thermal state and Low Power Mode of this Mac, or the `FASTRENDER_THERMAL` / `FASTRENDER_LOW_POWER` override.
/// Two property reads: hosts poll it about once a second instead of observing the notifications (no run loop on the
/// Electron render thread).
pub fn power_state() -> PowerState {
    if let Some(s) = PowerState::from_env() {
        return s;
    }
    let info = objc2_foundation::NSProcessInfo::processInfo();
    let thermal = match info.thermalState().0 {
        1 => Thermal::Fair,
        2 => Thermal::Serious,
        3 => Thermal::Critical,
        _ => Thermal::Nominal,
    };
    PowerState { thermal, low_power: info.isLowPowerModeEnabled() }
}

/// Polls `power_state` at most once a second and reports changes.
pub struct PowerWatch {
    pub state: PowerState,
    last: Option<Instant>,
    enabled: bool,
}

impl PowerWatch {
    /// `enabled` false (`--thermal 0`): the state stays nominal.
    pub fn new(enabled: bool) -> PowerWatch { PowerWatch { state: PowerState::default(), last: None, enabled } }

    /// Some(new state) when it changed since the last poll.
    pub fn poll(&mut self) -> Option<PowerState> {
        if !self.enabled || self.last.is_some_and(|t| t.elapsed().as_secs_f32() < 1.0) {
            return None;
        }
        self.last = Some(Instant::now());
        let s = power_state();
        if s == self.state {
            return None;
        }
        self.state = s;
        Some(s)
    }
}

/// `fastrender probe`: which GPU timestamp sources this device has (the per-pass timer design above).
pub fn probe_timers(dev: &Device) {
    let at = |p: MTLCounterSamplingPoint| dev.supportsCounterSampling(p);
    let sets: Vec<String> = dev.counterSets().map(|s| s.iter().map(|c| c.name().to_string()).collect()).unwrap_or_default();
    println!(
        "gpu timers: counter sampling at stage boundary: {}, draw boundary: {}, blit boundary: {}; counter sets: [{}]; shared event: {}",
        at(MTLCounterSamplingPoint::AtStageBoundary),
        at(MTLCounterSamplingPoint::AtDrawBoundary),
        at(MTLCounterSamplingPoint::AtBlitBoundary),
        sets.join(", "),
        dev.newSharedEvent().is_some()
    );
    println!("power: {}", power_state().label());
    println!("render thread QoS: {:?} (main thread before any change)", crate::qos::thread_qos());
}

impl Renderer {
    /// Frames in flight, slot waits and drawable waits since the last call.
    pub fn take_loop_stats(&mut self) -> LoopStats { std::mem::take(&mut self.loop_stats) }

    /// Completed frames' per-pass GPU ms since the last call (empty unless `cfg.gpu_passes`).
    pub fn take_pass_times(&self) -> Vec<[f64; 4]> { std::mem::take(&mut *self.pass_times.lock().unwrap()) }

    /// How many frames may be in flight.
    pub fn in_flight_limit(&self) -> usize { self.fence.limit() }

    /// "completion handler" or "shared event (n missed signals)"
    pub fn fence_label(&self) -> String {
        if self.fence.uses_event() { format!("shared event ({} missed signals)", self.fence.event_misses.get()) } else { "completion handler".into() }
    }

    /// Frame-rate cap for presented frames (thermal policy): each drawable stays on screen at least `1 / fps`
    /// (`presentDrawable:afterMinimumDuration:`), which back-pressures the loop through the drawable pool. None =
    /// present as soon as the display allows. A display link caps with `preferredFrameRateRange` instead.
    pub fn set_present_cap(&mut self, fps: Option<f32>) { self.present_min_s = fps.filter(|f| *f > 0.0).map(|f| 1.0 / f as f64); }

    /// May the layer's drawables be `framebufferOnly`? True when the frame's last pass is a render pass into the
    /// drawable (scale 1, or the bilinear upscale): the drawable is never sampled, blitted or written by compute.
    /// False while a MetalFX scaler writes the output (it asks for its own texture usage). Changes with the scale /
    /// upscale config, so hosts re-check it before taking a drawable; a readback of the drawable (Electron's `shot`)
    /// also needs it off.
    pub fn drawable_framebuffer_only(&self) -> bool { !self.targets.fx_writes_output() }
}
