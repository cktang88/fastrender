//! Cross-process channel between the game (Electron renderer process) and the Metal render thread
//! (Electron main process): POSIX shared memory + a named semaphore.
//!
//! Why: the NSView that must hold the CAMetalLayer belongs to the *browser* (main) process, a pointer
//! from `win.getNativeWindowHandle()` is meaningless in another process. So the main process owns the
//! view + the render thread, and the renderer publishes into this region.
//!
//! One shared region, two channels (docs/HOST.md "Spike"):
//! - **Frame mailbox** (droppable): `capacity_words` u32 words, single writer / single reader, seqlock.
//!   Only the newest frame matters, older ones are overwritten. Each frame also records
//!   `frame_res_seq`: the sequence number of the last resource message published before it.
//! - **Resource ring** (NOT droppable, ordered): a byte ring of `ring_bytes`. Upload/release messages
//!   (header + meta + array bytes) are streamed through it; the writer blocks (bounded) when it is full,
//!   so a message may be larger than the ring. The render thread applies every message up to the
//!   frame's `frame_res_seq` before drawing that frame.
//!
//! Recovery: when the renderer process dies or reloads, main calls `Slot::rotate`, which retires the old
//! region (wakes/aborts anything stuck on it, e.g. a seqlock left odd by a dead writer) and creates a
//! fresh one under the same name. Names are unlinked as soon as both sides attached, so a crash does
//! not leak `/fr-<pid>`.
//!
//! Portable (Linux/macOS) so everything here is unit-testable on Linux CI.

use std::ffi::CString;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const MAGIC_SHM: u32 = 0x4652_5348; // 'FRSH'
pub const MAGIC_FRAME2: u32 = 0x4652_4d32; // 'FRM2' (docs/PROTOCOL.md v2)
pub const MAGIC_FRAME3: u32 = 0x4652_4d33; // 'FRM3' (docs/PROTOCOL.md v3)
pub const HEADER_BYTES: usize = 256;
pub const DEFAULT_RING_BYTES: u32 = 4 << 20;
pub const MSG_HEADER_BYTES: usize = 24;
pub const OP_UPLOAD: u32 = 0;
pub const OP_RELEASE: u32 = 1;
/// fastrender mode (docs/HOST.md "Electron host"): codec in the fastrender crate, src/host.rs `op`
pub const OP_PROGRAM: u32 = 2;
pub const OP_UPLOAD_TYPED: u32 = 3;
pub const OP_FRAME: u32 = 4;
/// `Header::mode`: 0 = spike (frames through the mailbox, markers drawn), 1 = fastrender (frames through the ring,
/// rendered by the fastrender library)
pub const MODE_FASTRENDER: u32 = 1;
const MAX_MSG_BYTES: usize = 1 << 30;
const FNV_INIT: u32 = 0x811c_9dc5;

#[repr(C)]
pub struct Header {
    pub magic: u32,
    pub capacity_words: u32,
    /// frame seqlock: odd while the writer is copying
    pub seq: AtomicU32,
    pub words_used: AtomicU32,
    pub quit: AtomicU32,
    pub last_draw_count: AtomicU32,
    /// packed 0xRRGGBB of the last clear colour
    pub last_clear_rgb: AtomicU32,
    pub render_ready: AtomicU32,
    /// set by the writer process when it opened the region (main then unlinks the names)
    pub peer_attached: AtomicU32,
    /// set when this region was replaced (renderer died/reloaded): writers/readers must give up
    pub retired: AtomicU32,
    pub ring_bytes: u32,
    pub drawable_w: AtomicU32,
    pub drawable_h: AtomicU32,
    pub res_hash: AtomicU32,
    /// decode/order errors seen by the reader
    pub res_errors: AtomicU32,
    /// times a frame had to wait for its resources
    pub res_waits: AtomicU32,
    pub frames_published: AtomicU64,
    pub frames_rendered: AtomicU64,
    pub uploads: AtomicU64,
    pub upload_bytes: AtomicU64,
    /// total ns the render thread spent encoding+presenting (for averages)
    pub render_ns: AtomicU64,
    /// resource sequence number carried by the current frame (guarded by `seq`)
    pub frame_res_seq: AtomicU64,
    /// last resource message fully written (writer)
    pub res_seq_written: AtomicU64,
    /// last resource message applied (reader)
    pub res_applied_seq: AtomicU64,
    pub ring_head: AtomicU64,
    pub ring_tail: AtomicU64,
    /// cumulative (carried across rotations)
    pub res_applied_total: AtomicU64,
    pub res_bytes_applied: AtomicU64,
    // ---- fastrender mode (appended: the spike's layout above is unchanged)
    pub mode: AtomicU32,
    /// programs received by the render thread
    pub programs: AtomicU32,
    /// command buffers that completed with an error / resources the renderer rejected
    pub gpu_errors: AtomicU32,
    pub unsupported_resources: AtomicU32,
    /// GPU time of completed frames (us) and their count
    pub gpu_us: AtomicU64,
    pub gpu_frames: AtomicU64,
    /// screen-only frames dropped because a newer frame was already queued
    pub frames_skipped: AtomicU64,
    /// ring sequence number of the last FRAME message rendered (the game waits on it before a screenshot)
    pub last_frame_seq: AtomicU64,
    /// CPU time of the last frame's prepare + encode on the render thread (us)
    pub cpu_us_last: AtomicU64,
}

const _: () = assert!(std::mem::size_of::<Header>() <= HEADER_BYTES);

pub struct Shm {
    base: *mut u8,
    len: usize,
    sem: *mut libc::sem_t,
    pub name: String,
    pub owner: bool,
    linked: AtomicBool,
}

// SAFETY: the mapping and semaphore are process-wide handles; all shared fields are atomics and the
// payloads are guarded by the seqlock / ring head+tail protocol.
unsafe impl Send for Shm {}
unsafe impl Sync for Shm {}

fn sem_name(name: &str) -> CString {
    CString::new(format!("{name}-s")).unwrap()
}

/// FNV-1a over `bytes`, chained from `h`. The render thread hashes every applied resource message so
/// tests can assert content and order from JS (see game.mjs).
pub fn fnv_chain(mut h: u32, bytes: &[u8]) -> u32 {
    for &b in bytes {
        h = (h ^ b as u32).wrapping_mul(16_777_619);
    }
    h
}

impl Shm {
    /// Create (owner, main process). `name` like "/fr-1234" (macOS limits names to 31 chars).
    pub fn create(name: &str, capacity_words: u32, ring_bytes: u32) -> Result<Shm, String> {
        let cname = CString::new(name).map_err(|e| e.to_string())?;
        let len = HEADER_BYTES + capacity_words as usize * 4 + ring_bytes as usize;
        unsafe {
            libc::shm_unlink(cname.as_ptr());
            let fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR | libc::O_EXCL, 0o600);
            if fd < 0 {
                return Err(format!("shm_open(create {name}) failed: errno {}", *errno()));
            }
            if libc::ftruncate(fd, len as libc::off_t) != 0 {
                let e = *errno();
                libc::close(fd);
                libc::shm_unlink(cname.as_ptr());
                return Err(format!("ftruncate failed: errno {e}"));
            }
            let base = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
            libc::close(fd);
            if base == libc::MAP_FAILED {
                libc::shm_unlink(cname.as_ptr());
                return Err(format!("mmap failed: errno {}", *errno()));
            }
            let sn = sem_name(name);
            libc::sem_unlink(sn.as_ptr());
            let sem = libc::sem_open(sn.as_ptr(), libc::O_CREAT | libc::O_EXCL, 0o600 as libc::c_uint, 0 as libc::c_uint);
            if sem == libc::SEM_FAILED {
                let e = *errno();
                libc::munmap(base, len);
                libc::shm_unlink(cname.as_ptr());
                return Err(format!("sem_open(create) failed: errno {e}"));
            }
            let h = base as *mut Header;
            (*h).capacity_words = capacity_words;
            (*h).ring_bytes = ring_bytes;
            (*h).res_hash.store(FNV_INIT, Ordering::Relaxed);
            (*h).magic = MAGIC_SHM; // last: openers check it
            Ok(Shm { base: base as *mut u8, len, sem, name: name.to_string(), owner: true, linked: AtomicBool::new(true) })
        }
    }

    /// Open an existing region (renderer process). Tells the owner it may unlink the names.
    pub fn open(name: &str) -> Result<Shm, String> {
        let cname = CString::new(name).map_err(|e| e.to_string())?;
        unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_RDWR, 0o600);
            if fd < 0 {
                return Err(format!("shm_open(open {name}) failed: errno {} (sandboxed renderer? see docs/HOST.md)", *errno()));
            }
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut st) != 0 {
                libc::close(fd);
                return Err("fstat failed".into());
            }
            let len = st.st_size as usize;
            if len < HEADER_BYTES {
                libc::close(fd);
                return Err("shm too small".into());
            }
            let base = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
            libc::close(fd);
            if base == libc::MAP_FAILED {
                return Err(format!("mmap failed: errno {}", *errno()));
            }
            let h = &*(base as *const Header);
            if h.magic != MAGIC_SHM || len < HEADER_BYTES + h.capacity_words as usize * 4 + h.ring_bytes as usize {
                libc::munmap(base, len);
                return Err("shm magic/size mismatch".into());
            }
            let sn = sem_name(name);
            let sem = libc::sem_open(sn.as_ptr(), 0);
            if sem == libc::SEM_FAILED {
                let e = *errno();
                libc::munmap(base, len);
                return Err(format!("sem_open(open) failed: errno {e}"));
            }
            h.peer_attached.store(1, Ordering::Release);
            libc::sem_post(sem); // wake the owner so it can unlink the names
            Ok(Shm { base: base as *mut u8, len, sem, name: name.to_string(), owner: false, linked: AtomicBool::new(false) })
        }
    }

    pub fn header(&self) -> &Header {
        unsafe { &*(self.base as *const Header) }
    }

    pub fn capacity_words(&self) -> usize {
        self.header().capacity_words as usize
    }

    fn data(&self) -> *mut u32 {
        unsafe { self.base.add(HEADER_BYTES) as *mut u32 }
    }

    fn ring(&self) -> *mut u8 {
        unsafe { self.base.add(HEADER_BYTES + self.capacity_words() * 4) }
    }

    fn post(&self) {
        unsafe { libc::sem_post(self.sem) };
    }

    /// Remove the names (shm + semaphore) once both sides are attached: the mapping stays valid, and a
    /// crash can no longer leak them. Owner only, idempotent.
    pub fn unlink_names(&self) {
        if self.owner && self.linked.swap(false, Ordering::AcqRel) {
            let c = CString::new(self.name.clone()).unwrap();
            unsafe {
                libc::shm_unlink(c.as_ptr());
                libc::sem_unlink(sem_name(&self.name).as_ptr());
            }
        }
    }

    pub fn is_linked(&self) -> bool {
        self.linked.load(Ordering::Acquire)
    }

    pub fn peer_attached(&self) -> bool {
        self.header().peer_attached.load(Ordering::Acquire) != 0
    }

    pub fn retired(&self) -> bool {
        self.header().retired.load(Ordering::Acquire) != 0
    }

    /// Mark this region dead and wake the reader. Writers/readers give up on it.
    pub fn retire(&self) {
        self.header().retired.store(1, Ordering::Release);
        self.post();
    }

    // ----------------------------------------------------------------------------- frame mailbox

    /// Writer side: publish `words` (the used prefix of the command buffer) and wake the renderer.
    /// The frame is stamped with the resource sequence number written so far.
    pub fn publish(&self, words: &[u32]) -> Result<(), String> {
        let h = self.header();
        if self.retired() {
            return Err("host channel was reset (renderer reloaded?); re-attach".into());
        }
        if h.mode.load(Ordering::Acquire) == MODE_FASTRENDER {
            // ordered with the resources and never dropped here: the render thread decides which frames to skip
            // (a frame rendering into a target always runs, see fr_queue.rs)
            let bytes = unsafe { std::slice::from_raw_parts(words.as_ptr() as *const u8, words.len() * 4) };
            self.send_resource(OP_FRAME, 0, 0, &[bytes])?;
            h.frames_published.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        if words.len() > self.capacity_words() {
            return Err(format!("frame of {} words exceeds mailbox capacity {}", words.len(), self.capacity_words()));
        }
        let s = h.seq.load(Ordering::Relaxed);
        let odd = s.wrapping_add(1) | 1;
        h.seq.store(odd, Ordering::Release); // odd: writing
        std::sync::atomic::fence(Ordering::SeqCst);
        unsafe { std::ptr::copy_nonoverlapping(words.as_ptr(), self.data(), words.len()) };
        h.words_used.store(words.len() as u32, Ordering::Relaxed);
        h.frame_res_seq.store(h.res_seq_written.load(Ordering::Acquire), Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::SeqCst);
        h.seq.store(odd.wrapping_add(1), Ordering::Release); // even: stable
        h.frames_published.fetch_add(1, Ordering::Relaxed);
        self.post();
        Ok(())
    }

    /// Reader side: block until posted (frame, resource bytes, attach, retire or quit).
    fn wait(&self) {
        loop {
            if unsafe { libc::sem_wait(self.sem) } == 0 || unsafe { *errno() } != libc::EINTR {
                return;
            }
        }
    }

    /// Reader side: swallow extra pending wakeups (the caller rescans all state afterwards).
    fn drain_sem(&self) {
        while unsafe { libc::sem_trywait(self.sem) } == 0 {}
    }

    /// Wake the reader with the quit flag set.
    pub fn request_quit(&self) {
        self.header().quit.store(1, Ordering::Release);
        self.post();
    }

    /// Reader side: copy the latest consistent frame into `out`. Returns `(seq, resource_seq)`.
    /// The spin is bounded (and abandoned on quit/retire): a writer that died mid-copy leaves `seq`
    /// odd forever, which must not hang the render thread or `shutdown()`.
    pub fn snapshot(&self, out: &mut Vec<u32>) -> Option<(u32, u64)> {
        let h = self.header();
        let deadline = Instant::now() + Duration::from_millis(5);
        let mut spins = 0u32;
        loop {
            let s1 = h.seq.load(Ordering::Acquire);
            if s1 & 1 == 0 {
                let n = (h.words_used.load(Ordering::Relaxed) as usize).min(self.capacity_words());
                let res = h.frame_res_seq.load(Ordering::Relaxed);
                out.clear();
                out.reserve(n);
                // copy only the used words (no zero-fill); torn data is discarded by the seq re-check
                unsafe {
                    std::ptr::copy_nonoverlapping(self.data(), out.as_mut_ptr(), n);
                    out.set_len(n);
                }
                std::sync::atomic::fence(Ordering::SeqCst);
                if h.seq.load(Ordering::Acquire) == s1 {
                    return Some((s1, res));
                }
            }
            spins += 1;
            if spins % 64 == 0 {
                if h.quit.load(Ordering::Relaxed) != 0 || self.retired() || Instant::now() >= deadline {
                    return None;
                }
                std::thread::yield_now();
            } else {
                std::hint::spin_loop();
            }
        }
    }

    // ----------------------------------------------------------------------------- resource ring

    /// Writer side: append one message to the ring and return its sequence number. `parts` are
    /// concatenated into the payload. Blocks (bounded: 2 s without progress) when the ring is full, so a
    /// message may exceed the ring size. Never drops.
    pub fn send_resource(&self, op: u32, kind: u32, id: u32, parts: &[&[u8]]) -> Result<u64, String> {
        let h = self.header();
        let payload: usize = parts.iter().map(|p| p.len()).sum();
        if payload > MAX_MSG_BYTES {
            return Err("resource message too large".into());
        }
        let seq = h.res_seq_written.load(Ordering::Relaxed) + 1;
        let mut hdr = [0u8; MSG_HEADER_BYTES];
        hdr[0..4].copy_from_slice(&(payload as u32).to_le_bytes());
        hdr[4..8].copy_from_slice(&op.to_le_bytes());
        hdr[8..12].copy_from_slice(&kind.to_le_bytes());
        hdr[12..16].copy_from_slice(&id.to_le_bytes());
        hdr[16..24].copy_from_slice(&seq.to_le_bytes());
        self.ring_write(&hdr)?;
        for p in parts {
            self.ring_write(p)?;
        }
        h.res_seq_written.store(seq, Ordering::Release);
        if op == OP_UPLOAD || op == OP_UPLOAD_TYPED {
            h.uploads.fetch_add(1, Ordering::Relaxed);
            h.upload_bytes.fetch_add(payload as u64, Ordering::Relaxed);
        }
        self.post();
        Ok(seq)
    }

    fn ring_write(&self, mut data: &[u8]) -> Result<(), String> {
        let h = self.header();
        let cap = h.ring_bytes as u64;
        let mut stalled: Option<Instant> = None;
        while !data.is_empty() {
            if self.retired() || h.quit.load(Ordering::Relaxed) != 0 {
                return Err("host channel closed or reset while uploading".into());
            }
            let head = h.ring_head.load(Ordering::Relaxed);
            let tail = h.ring_tail.load(Ordering::Acquire);
            let free = cap - (head - tail);
            if free == 0 {
                let t0 = *stalled.get_or_insert_with(Instant::now);
                if t0.elapsed() > Duration::from_secs(2) {
                    return Err("resource ring full for 2 s: render thread stalled".into());
                }
                self.post();
                std::thread::sleep(Duration::from_micros(50));
                continue;
            }
            stalled = None;
            let n = (free as usize).min(data.len());
            let off = (head % cap) as usize;
            let first = n.min(cap as usize - off);
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), self.ring().add(off), first);
                if n > first {
                    std::ptr::copy_nonoverlapping(data.as_ptr().add(first), self.ring(), n - first);
                }
            }
            h.ring_head.store(head + n as u64, Ordering::Release);
            data = &data[n..];
            if !data.is_empty() {
                self.post(); // let the reader make room while we are still writing
            }
        }
        Ok(())
    }

    /// Reader side: append up to `max` available ring bytes to `dst`. Returns the count.
    fn ring_read_into(&self, dst: &mut Vec<u8>, max: usize) -> usize {
        let h = self.header();
        let cap = h.ring_bytes as u64;
        let tail = h.ring_tail.load(Ordering::Relaxed);
        let head = h.ring_head.load(Ordering::Acquire);
        let n = ((head - tail) as usize).min(max);
        if n == 0 {
            return 0;
        }
        let off = (tail % cap) as usize;
        let first = n.min(cap as usize - off);
        dst.reserve(n);
        unsafe {
            let p = dst.as_mut_ptr().add(dst.len());
            std::ptr::copy_nonoverlapping(self.ring().add(off), p, first);
            if n > first {
                std::ptr::copy_nonoverlapping(self.ring(), p.add(first), n - first);
            }
            dst.set_len(dst.len() + n);
        }
        h.ring_tail.store(tail + n as u64, Ordering::Release);
        n
    }

    /// Discard everything in the ring (corrupt stream recovery).
    fn ring_discard(&self) {
        let h = self.header();
        h.ring_tail.store(h.ring_head.load(Ordering::Acquire), Ordering::Release);
    }

    /// Carry cumulative statistics over from a retired region.
    fn carry_from(&self, old: &Shm) {
        let (n, o) = (self.header(), old.header());
        for (a, b) in [(&n.frames_published, &o.frames_published), (&n.frames_rendered, &o.frames_rendered), (&n.uploads, &o.uploads),
            (&n.upload_bytes, &o.upload_bytes), (&n.render_ns, &o.render_ns), (&n.res_applied_total, &o.res_applied_total),
            (&n.res_bytes_applied, &o.res_bytes_applied), (&n.gpu_us, &o.gpu_us), (&n.gpu_frames, &o.gpu_frames),
            (&n.frames_skipped, &o.frames_skipped)] {
            a.store(b.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        for (a, b) in [(&n.last_draw_count, &o.last_draw_count), (&n.last_clear_rgb, &o.last_clear_rgb), (&n.render_ready, &o.render_ready),
            (&n.drawable_w, &o.drawable_w), (&n.drawable_h, &o.drawable_h), (&n.res_hash, &o.res_hash), (&n.res_errors, &o.res_errors),
            (&n.res_waits, &o.res_waits), (&n.mode, &o.mode), (&n.programs, &o.programs), (&n.gpu_errors, &o.gpu_errors),
            (&n.unsupported_resources, &o.unsupported_resources)] {
            a.store(b.load(Ordering::Relaxed), Ordering::Relaxed);
        }
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        self.unlink_names();
        unsafe {
            libc::sem_close(self.sem);
            libc::munmap(self.base as *mut libc::c_void, self.len);
        }
    }
}

#[cfg(target_os = "macos")]
unsafe fn errno() -> *mut i32 {
    unsafe { libc::__error() }
}
#[cfg(not(target_os = "macos"))]
unsafe fn errno() -> *mut i32 {
    unsafe { libc::__errno_location() }
}

// ---------------------------------------------------------------------------------------------
// Owner-side handle shared by the main-process JS thread and the render thread.

pub struct Slot {
    cur: Mutex<Arc<Shm>>,
    quit: AtomicBool,
}

impl Slot {
    pub fn new(shm: Arc<Shm>) -> Arc<Slot> {
        Arc::new(Slot { cur: Mutex::new(shm), quit: AtomicBool::new(false) })
    }

    pub fn current(&self) -> Arc<Shm> {
        self.cur.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn quitting(&self) -> bool {
        self.quit.load(Ordering::Acquire)
    }

    /// Never blocks: sets the flags and wakes the render thread.
    pub fn request_quit(&self) {
        self.quit.store(true, Ordering::Release);
        self.current().request_quit();
    }

    /// The renderer died or navigated: retire the current region (aborting any writer/seqlock stuck on
    /// it) and replace it with a fresh one under the same name. Cumulative stats carry over.
    pub fn rotate(&self) -> Result<Arc<Shm>, String> {
        let mut g = self.cur.lock().unwrap_or_else(|e| e.into_inner());
        let old = g.clone();
        old.retire();
        old.unlink_names();
        let h = old.header();
        let new = Arc::new(Shm::create(&old.name, h.capacity_words, h.ring_bytes)?);
        new.carry_from(&old);
        *g = new.clone();
        old.post();
        Ok(new)
    }
}

// ---------------------------------------------------------------------------------------------
// Render-thread side: applies resource messages in order, then yields the newest frame.

/// Receives every decoded message body: (op, kind, id, seq, body). fastrender mode turns them into renderer input.
pub type Sink = Box<dyn FnMut(u32, u32, u32, u64, &[u8]) + Send>;

/// What `Consumer::pump` saw.
#[derive(Debug, PartialEq, Eq)]
pub enum Pump {
    Quit,
    /// the region was replaced (page reloaded / renderer crashed): the next messages belong to a new page
    Rotated,
    Messages,
}

pub struct Consumer {
    /// fastrender mode: every message goes here after validation
    pub sink: Option<Sink>,
    slot: Arc<Slot>,
    shm: Arc<Shm>,
    pending: Vec<u8>,
    applied: u64,
    last_seq: u32,
    /// the newest frame, valid after `next_frame()` returned true
    pub words: Vec<u32>,
}

impl Consumer {
    pub fn new(slot: Arc<Slot>) -> Consumer {
        let shm = slot.current();
        Consumer { sink: None, slot, shm, pending: Vec::new(), applied: 0, last_seq: 0, words: Vec::new() }
    }

    /// Header of the region currently in use (changes after a rotation).
    pub fn header(&self) -> &Header {
        self.shm.header()
    }

    fn adopt(&mut self) -> bool {
        let cur = self.slot.current();
        if !Arc::ptr_eq(&cur, &self.shm) {
            self.shm = cur;
            self.pending.clear();
            self.applied = 0;
            self.last_seq = 0;
            return true;
        }
        false
    }

    /// fastrender mode (reader thread): block until messages were decoded (they went to `sink`), the region was
    /// rotated, or quit. Frames travel in the ring too, so there is no mailbox to look at.
    pub fn pump(&mut self) -> Pump {
        loop {
            if self.adopt() {
                return Pump::Rotated;
            }
            if self.slot.quitting() || self.shm.header().quit.load(Ordering::Acquire) != 0 {
                return Pump::Quit;
            }
            if !self.shm.retired() {
                if self.shm.is_linked() && self.shm.peer_attached() {
                    self.shm.unlink_names();
                }
                if self.drain_resources() {
                    return Pump::Messages;
                }
            }
            self.shm.wait();
            self.shm.drain_sem();
        }
    }

    /// Block until there is a new frame whose resources are applied. Returns false on quit.
    pub fn next_frame(&mut self) -> bool {
        loop {
            self.adopt();
            if self.slot.quitting() || self.shm.header().quit.load(Ordering::Acquire) != 0 {
                return false;
            }
            if !self.shm.retired() {
                if self.shm.is_linked() && self.shm.peer_attached() {
                    self.shm.unlink_names();
                }
                self.drain_resources();
                let s = self.shm.header().seq.load(Ordering::Acquire);
                if s & 1 == 0 && s != self.last_seq {
                    if let Some((seq, res)) = self.shm.snapshot(&mut self.words) {
                        self.last_seq = seq;
                        self.apply_until(res);
                        return true;
                    }
                }
            }
            self.shm.wait();
            self.shm.drain_sem();
        }
    }

    /// Apply every message that is already in the ring; false if there was nothing.
    pub fn drain_resources(&mut self) -> bool {
        let mut any = false;
        loop {
            let want = if self.pending.len() < MSG_HEADER_BYTES {
                MSG_HEADER_BYTES - self.pending.len()
            } else {
                let len = u32::from_le_bytes(self.pending[0..4].try_into().unwrap()) as usize;
                if len > MAX_MSG_BYTES {
                    self.shm.header().res_errors.fetch_add(1, Ordering::Relaxed);
                    self.pending.clear();
                    self.shm.ring_discard();
                    return any;
                }
                MSG_HEADER_BYTES + len - self.pending.len()
            };
            if want == 0 {
                self.decode();
                self.pending.clear();
                any = true;
                continue;
            }
            if self.shm.ring_read_into(&mut self.pending, want) == 0 {
                return any;
            }
        }
    }

    /// A frame depends on resource messages up to `res`: they were fully written before the frame was
    /// published, so they are in the ring; wait (bounded) only for the reader's own lag.
    fn apply_until(&mut self, res: u64) {
        let deadline = Instant::now() + Duration::from_millis(200);
        let mut waited = false;
        while self.applied < res {
            self.drain_resources();
            if self.applied >= res {
                break;
            }
            waited = true;
            if Instant::now() >= deadline || self.shm.retired() {
                self.shm.header().res_errors.fetch_add(1, Ordering::Relaxed);
                break;
            }
            std::thread::yield_now();
        }
        if waited {
            self.shm.header().res_waits.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn decode(&mut self) {
        let h = self.shm.header();
        let m = &self.pending;
        let len = u32::from_le_bytes(m[0..4].try_into().unwrap()) as usize;
        let op = u32::from_le_bytes(m[4..8].try_into().unwrap());
        let kind = u32::from_le_bytes(m[8..12].try_into().unwrap());
        let id = u32::from_le_bytes(m[12..16].try_into().unwrap());
        let seq = u64::from_le_bytes(m[16..24].try_into().unwrap());
        let body = &m[MSG_HEADER_BYTES..];
        debug_assert_eq!(body.len(), len);
        let mut bad = seq != self.applied + 1;
        let mut hash = fnv_chain(h.res_hash.load(Ordering::Relaxed), &kind.to_le_bytes());
        hash = fnv_chain(hash, &id.to_le_bytes());
        match op {
            OP_UPLOAD | OP_UPLOAD_TYPED => {
                // [meta_len u32][n u32][len_i u32 x n]([type_i u32 x n] typed)[meta][array_0]...[array_n-1]
                let words_per = if op == OP_UPLOAD_TYPED { 2 } else { 1 };
                let rd = |o: usize| body.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
                let parsed = (|| {
                    let meta_len = rd(0)?;
                    let n = rd(4)?;
                    let mut off = 8usize.checked_add(n.checked_mul(4 * words_per)?)?;
                    let lens: Vec<usize> = (0..n).map(|i| rd(8 + i * 4)).collect::<Option<_>>()?;
                    let meta = body.get(off..off.checked_add(meta_len)?)?;
                    off += meta_len;
                    let mut hash = fnv_chain(hash, meta);
                    for l in lens {
                        let a = body.get(off..off.checked_add(l)?)?;
                        hash = fnv_chain(hash, &(l as u32).to_le_bytes());
                        hash = fnv_chain(hash, a);
                        off += l;
                    }
                    (off == body.len()).then_some(hash)
                })();
                match parsed {
                    Some(hh) => hash = hh,
                    None => bad = true,
                }
            }
            OP_RELEASE | OP_PROGRAM | OP_FRAME => {}
            _ => bad = true,
        }
        if !bad {
            if let Some(sink) = self.sink.as_mut() {
                sink(op, kind, id, seq, body);
            }
        }
        if bad {
            h.res_errors.fetch_add(1, Ordering::Relaxed);
        }
        h.res_hash.store(hash, Ordering::Relaxed);
        h.res_bytes_applied.fetch_add(len as u64, Ordering::Relaxed);
        h.res_applied_total.fetch_add(1, Ordering::Relaxed);
        self.applied = seq;
        h.res_applied_seq.store(seq, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------------------------
// Frame stream parsing (docs/PROTOCOL.md section 2)

#[derive(Debug, Default, Clone, PartialEq)]
pub struct FrameInfo {
    pub valid: bool,
    pub draw_count: u32,
    /// BACKGROUND command rgb (op 4), if present
    pub background: Option<[f32; 3]>,
}

pub fn parse_frame(w: &[u32]) -> FrameInfo {
    let mut fi = FrameInfo::default();
    if w.len() < 2 || (w[0] != MAGIC_FRAME2 && w[0] != MAGIC_FRAME3) {
        return fi;
    }
    let used = (w[1] as usize).min(w.len());
    let mut i = 2;
    while i + 2 <= used {
        let op = w[i];
        let n = w[i + 1] as usize;
        let p = i + 2;
        if p + n > used {
            return fi;
        }
        match op {
            4 if n >= 3 => fi.background = Some([f32::from_bits(w[p]), f32::from_bits(w[p + 1]), f32::from_bits(w[p + 2])]),
            5 if n >= 1 => fi.draw_count = w[p],
            _ => {}
        }
        i = p + n;
    }
    fi.valid = true;
    fi
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_m(magic: u32, bg: [f32; 3], draws: u32) -> Vec<u32> {
        let mut v = vec![magic, 0, 4, 4, bg[0].to_bits(), bg[1].to_bits(), bg[2].to_bits(), 1f32.to_bits()];
        v.extend([5, 1 + draws * 20, draws]);
        v.extend(std::iter::repeat(0).take((draws * 20) as usize));
        v[1] = v.len() as u32;
        v
    }
    fn frame(bg: [f32; 3], draws: u32) -> Vec<u32> {
        frame_m(MAGIC_FRAME2, bg, draws)
    }
    fn uname(tag: &str) -> String {
        format!("/frt-{}-{tag}", std::process::id() % 100000)
    }
    /// JS-side layout of an upload payload (host: lib.rs `upload`).
    fn upload_parts(meta: &str, arrays: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let mut prefix = Vec::new();
        prefix.extend((meta.len() as u32).to_le_bytes());
        prefix.extend((arrays.len() as u32).to_le_bytes());
        for a in arrays {
            prefix.extend((a.len() as u32).to_le_bytes());
        }
        prefix.extend(meta.as_bytes());
        let mut v = vec![prefix];
        v.extend(arrays.iter().cloned());
        v
    }
    fn expect_hash(mut h: u32, kind: u32, id: u32, meta: &str, arrays: &[Vec<u8>]) -> u32 {
        h = fnv_chain(h, &kind.to_le_bytes());
        h = fnv_chain(h, &id.to_le_bytes());
        h = fnv_chain(h, meta.as_bytes());
        for a in arrays {
            h = fnv_chain(h, &(a.len() as u32).to_le_bytes());
            h = fnv_chain(h, a);
        }
        h
    }

    #[test]
    fn parses_frm2_and_frm3() {
        let f = parse_frame(&frame([0.25, 0.5, 0.75], 7));
        assert!(f.valid);
        assert_eq!(f.draw_count, 7);
        assert_eq!(f.background, Some([0.25, 0.5, 0.75]));
        let f3 = parse_frame(&frame_m(MAGIC_FRAME3, [0.1, 0.2, 0.3], 2));
        assert!(f3.valid && f3.draw_count == 2);
        assert!(!parse_frame(&[1, 2, 3]).valid);
        assert!(!parse_frame(&[0x4652_4d34, 2]).valid);
    }

    #[test]
    fn mailbox_roundtrip_two_handles() {
        let name = uname("rt");
        let a = Arc::new(Shm::create(&name, 1024, 4096).unwrap());
        let b = Shm::open(&name).unwrap(); // same process, separate mapping: like renderer vs main
        let slot = Slot::new(a.clone());
        let mut c = Consumer::new(slot.clone());
        let fr = frame([1.0, 0.0, 0.5], 3);
        b.publish(&fr).unwrap();
        assert!(c.next_frame());
        assert_eq!(c.words, fr);
        assert!(a.is_linked() == false, "names are unlinked once both sides attached");
        assert_eq!(a.header().frames_published.load(Ordering::Relaxed), 1);
        assert!(b.publish(&vec![0; 2000]).is_err());
        slot.request_quit();
        assert!(!c.next_frame());
    }

    #[test]
    fn resource_channel_in_order_bigger_than_ring_and_frame_dependency() {
        let name = uname("res");
        let a = Arc::new(Shm::create(&name, 2048, 1000).unwrap()); // tiny ring: forces blocking + wrap
        let w = Shm::open(&name).unwrap();
        let slot = Slot::new(a.clone());
        let slot2 = slot.clone();
        let reader = std::thread::spawn(move || {
            let mut c = Consumer::new(slot2);
            let mut frames = Vec::new();
            while c.next_frame() {
                let h = c.header();
                // the contract: when a frame is returned, all resources it depends on are applied
                frames.push((c.words.clone(), h.res_applied_seq.load(Ordering::Acquire)));
                if parse_frame(&c.words).draw_count == 42 {
                    break;
                }
            }
            (frames, c.header().res_hash.load(Ordering::Relaxed), c.header().res_errors.load(Ordering::Relaxed))
        });
        let mut h = FNV_INIT;
        let mut seqs = Vec::new();
        for i in 0u32..40 {
            let n = if i == 7 { 5000 } else { 1 + (i as usize * 37) % 300 };
            let arrays = vec![(0..n).map(|k| (k as u32 ^ i) as u8).collect::<Vec<u8>>(), vec![i as u8; 3]];
            let meta = format!("{{\"i\":{i}}}");
            let parts = upload_parts(&meta, &arrays);
            let refs: Vec<&[u8]> = parts.iter().map(|p| p.as_slice()).collect();
            seqs.push(w.send_resource(OP_UPLOAD, 2, i, &refs).unwrap());
            h = expect_hash(h, 2, i, &meta, &arrays);
            if i % 15 == 14 {
                w.publish(&frame([0.0, 0.0, 0.0], i)).unwrap();
            }
        }
        assert_eq!(seqs, (1..=40).collect::<Vec<u64>>());
        let rel = w.send_resource(OP_RELEASE, 2, 5, &[]).unwrap();
        h = fnv_chain(fnv_chain(h, &2u32.to_le_bytes()), &5u32.to_le_bytes());
        assert_eq!(rel, 41);
        w.publish(&frame([0.0, 0.0, 0.0], 41)).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        w.publish(&frame([0.0, 0.0, 0.0], 42)).unwrap();
        let (frames, hash, errs) = reader.join().unwrap();
        assert_eq!(errs, 0);
        assert_eq!(hash, h, "bytes and order of all 41 messages match");
        let last = frames.last().unwrap();
        assert_eq!(last.1, 41);
        for (f, applied) in &frames {
            let draws = parse_frame(f).draw_count as u64;
            assert!(*applied >= draws.min(41), "frame with {draws} draws saw only {applied} resources");
        }
    }

    #[test]
    fn dead_writer_odd_seq_does_not_hang_and_rotate_recovers() {
        let name = uname("dead");
        let a = Arc::new(Shm::create(&name, 256, 4096).unwrap());
        let w = Shm::open(&name).unwrap();
        w.publish(&frame([0.0; 3], 1)).unwrap();
        w.header().seq.store(w.header().seq.load(Ordering::Relaxed) | 1, Ordering::Release); // writer died mid-copy
        let mut out = Vec::new();
        let t = Instant::now();
        assert!(a.snapshot(&mut out).is_none());
        assert!(t.elapsed() < Duration::from_millis(500));
        let slot = Slot::new(a.clone());
        let mut c = Consumer::new(slot.clone());
        // a rotation (render-process-gone) gives a clean region; a new renderer attaches and publishes
        let n = slot.rotate().unwrap();
        assert!(a.retired());
        assert!(w.publish(&frame([0.0; 3], 1)).is_err(), "old writer is told to re-attach");
        let w2 = Shm::open(&n.name).unwrap();
        w2.publish(&frame([0.5; 3], 9)).unwrap();
        assert!(c.next_frame());
        assert_eq!(parse_frame(&c.words).draw_count, 9);
        // a reader blocked in wait() is released by quit; shutdown never hangs
        let slot2 = slot.clone();
        let t = std::thread::spawn(move || {
            let mut c = Consumer::new(slot2);
            while c.next_frame() {}
        });
        std::thread::sleep(Duration::from_millis(50));
        slot.request_quit();
        t.join().unwrap();
    }
}
