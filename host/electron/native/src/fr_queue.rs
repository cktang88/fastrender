//! fastrender mode, main process: between the ring reader and the render thread.
//!
//! A reader thread (`spawn_reader`) drains the shared-memory resource ring as fast as the renderer process writes it
//! (so `upload`/`present` never stall behind a render thread busy compiling pipelines) and decodes every message
//! into an [`Item`] (fastrender's wire codec, src/host.rs). The render thread takes one [`Batch`] per rendered frame:
//! every resource and program in ring order up to that frame. A screen-only frame is skipped when a newer frame is
//! already queued (newest wins, as the spike's mailbox did); a frame with a pass into a render target always runs
//! (PMREM / captures at load: later frames sample what it writes). Portable: unit-tested on Linux.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

use fastrender::host::{decode_frame, decode_program, decode_upload, frame_has_offscreen_pass};
use fastrender::three::Resource;
use fastrender::xlat::Program;

use crate::shm::{Consumer, OP_FRAME, OP_PROGRAM, OP_RELEASE, OP_UPLOAD_TYPED, Pump, Slot};

pub enum Item {
    /// the page reloaded or its process died: everything before belongs to the old page
    Reset,
    Resource(Resource),
    Program(u32, String, Program),
    Frame { words: Vec<u32>, seq: u64 },
}

#[derive(Default)]
pub struct Queue {
    items: VecDeque<Item>,
    frames: usize,
    quit: bool,
    pub decode_errors: u32,
}

#[derive(Default)]
pub struct Shared {
    q: Mutex<Queue>,
    cv: Condvar,
}

/// One frame to render and what it needs.
#[derive(Default)]
pub struct Batch {
    /// the renderer must start over (new page) before applying this batch
    pub reset: bool,
    pub resources: Vec<Resource>,
    pub programs: Vec<(u32, String, Program)>,
    pub words: Vec<u32>,
    /// ring sequence number of the FRAME message
    pub seq: u64,
    /// screen-only frames dropped in favour of this one
    pub skipped: u64,
}

impl Shared {
    pub fn new() -> Arc<Shared> {
        Arc::new(Shared::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.q.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn push(&self, it: Item) {
        let mut q = self.lock();
        if matches!(it, Item::Frame { .. }) {
            q.frames += 1;
        }
        let wake = matches!(it, Item::Frame { .. } | Item::Reset);
        q.items.push_back(it);
        drop(q);
        if wake {
            self.cv.notify_all();
        }
    }

    pub fn decode_error(&self, e: String) {
        eprintln!("fastrender host: {e}");
        self.lock().decode_errors += 1;
    }

    pub fn quit(&self) {
        self.lock().quit = true;
        self.cv.notify_all();
    }

    #[cfg(test)]
    pub fn decode_errors(&self) -> u32 {
        self.lock().decode_errors
    }

    /// Render thread: block until a frame is queued; None on quit.
    pub fn next_batch(&self) -> Option<Batch> {
        let mut q = self.lock();
        let mut b = Batch::default();
        loop {
            while let Some(it) = q.items.pop_front() {
                match it {
                    Item::Reset => {
                        b = Batch { reset: true, ..Batch::default() };
                    }
                    Item::Resource(r) => b.resources.push(r),
                    Item::Program(id, key, p) => b.programs.push((id, key, p)),
                    Item::Frame { words, seq } => {
                        q.frames -= 1;
                        if q.frames > 0 && !frame_has_offscreen_pass(&words) {
                            b.skipped += 1;
                            continue;
                        }
                        b.words = words;
                        b.seq = seq;
                        return Some(b);
                    }
                }
            }
            if q.quit {
                return None;
            }
            q = self.cv.wait(q).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Decode one ring message into the queue (the `Consumer` sink).
pub fn sink_into(shared: Arc<Shared>) -> crate::shm::Sink {
    Box::new(move |op, kind, id, seq, body| match op {
        OP_UPLOAD_TYPED => match decode_upload(kind, id, body) {
            Ok(r) => shared.push(Item::Resource(r)),
            Err(e) => shared.decode_error(e),
        },
        OP_RELEASE => shared.push(Item::Resource(Resource::Release { kind, id })),
        OP_PROGRAM => match decode_program(body) {
            Ok((key, p)) => shared.push(Item::Program(id, key, p)),
            Err(e) => shared.decode_error(e),
        },
        OP_FRAME => shared.push(Item::Frame { words: decode_frame(body), seq }),
        _ => {} // the spike's untyped uploads: hashed only
    })
}

/// The ring reader thread of fastrender mode.
pub fn spawn_reader(slot: Arc<Slot>, shared: Arc<Shared>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("fastrender-ring".into())
        .spawn(move || {
            let mut c = Consumer::new(slot);
            c.sink = Some(sink_into(shared.clone()));
            c.header().render_ready.store(1, std::sync::atomic::Ordering::Release);
            loop {
                match c.pump() {
                    Pump::Quit => break,
                    Pump::Rotated => shared.push(Item::Reset),
                    Pump::Messages => {}
                }
            }
            shared.quit();
        })
        .expect("spawn ring reader")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shm::{MODE_FASTRENDER, Shm};
    use std::sync::atomic::Ordering;

    fn frame(target: u32, tag: u32) -> Vec<u32> {
        let mut f = vec![0x4652_4d33, 0, fastrender::three::OP_PASS, 24, target];
        f.extend([0u32; 23]);
        f.extend([99, 1, tag]);
        f[1] = f.len() as u32;
        f
    }

    fn typed_upload(w: &Shm, kind: u32, id: u32, data: &[f32]) {
        let bytes = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 4) };
        let meta = "{}";
        let mut prefix = fastrender::host::upload_prefix(meta.len(), &[(fastrender::host::ty::F32, bytes.len())]);
        prefix.extend(meta.as_bytes());
        w.send_resource(OP_UPLOAD_TYPED, kind, id, &[&prefix, bytes]).unwrap();
    }

    #[test]
    fn frames_through_the_ring_skip_screen_only_keep_offscreen() {
        let name = format!("/frq-{}", std::process::id() % 100000);
        let a = Arc::new(Shm::create(&name, 16, 512).unwrap()); // tiny ring: messages larger than it stream through
        a.header().mode.store(MODE_FASTRENDER, Ordering::Release);
        let w = Shm::open(&name).unwrap();
        let slot = Slot::new(a.clone());
        let shared = Shared::new();
        let reader = spawn_reader(slot.clone(), shared.clone());
        typed_upload(&w, 1, 10, &[1.0; 300]);
        w.publish(&frame(7, 1)).unwrap(); // into a render target: must run
        w.publish(&frame(0, 2)).unwrap(); // screen only, superseded: skipped
        w.send_resource(OP_RELEASE, 1, 10, &[]).unwrap();
        w.publish(&frame(0, 3)).unwrap();
        // wait until the reader queued all three frames
        for _ in 0..500 {
            if shared.lock().frames == 3 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let b1 = shared.next_batch().unwrap();
        assert_eq!(b1.resources.len(), 1);
        assert!(matches!(&b1.resources[0], Resource::Upload { kind: 1, id: 10, arrays, .. } if matches!(&arrays[0], Some(fastrender::three::Array::F32(v)) if v.len() == 300)));
        assert_eq!(*b1.words.last().unwrap(), 1);
        let b2 = shared.next_batch().unwrap();
        assert_eq!((b2.skipped, *b2.words.last().unwrap()), (1, 3));
        assert!(matches!(b2.resources[..], [Resource::Release { kind: 1, id: 10 }]));
        assert_eq!(a.header().frames_published.load(Ordering::Relaxed), 3);
        // a reload: Reset reaches the render thread before the new page's first frame
        let n = slot.rotate().unwrap();
        let w2 = Shm::open(&n.name).unwrap();
        w2.publish(&frame(0, 4)).unwrap();
        let b3 = shared.next_batch().unwrap();
        assert!(b3.reset);
        assert_eq!(*b3.words.last().unwrap(), 4);
        slot.request_quit();
        reader.join().unwrap();
        assert!(shared.next_batch().is_none());
        assert_eq!(shared.decode_errors(), 0);
    }
}
