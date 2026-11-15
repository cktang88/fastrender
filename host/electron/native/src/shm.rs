//! Cross-process frame mailbox: POSIX shared memory + a named semaphore.
//!
//! Why: in Electron the game (FastRenderer) runs in the *renderer* process, but the NSView that must
//! hold the CAMetalLayer belongs to the *browser* (main) process. A pointer from
//! `win.getNativeWindowHandle()` is meaningless in another process. So the main process owns the
//! view + the Metal render thread, and the renderer publishes each frame into this mailbox (one
//! memcpy of the command stream, ~100 KB = a few microseconds), then posts the semaphore.
//!
//! Layout: `Header` (128 bytes) followed by `capacity_words` u32 words.
//! Single writer (renderer), single reader (render thread); a seqlock makes the snapshot consistent.
//! Portable (Linux/macOS) so the protocol parsing is unit-testable on Linux CI.

use std::ffi::CString;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

pub const MAGIC_SHM: u32 = 0x4652_5348; // 'FRSH'
pub const MAGIC_FRAME: u32 = 0x4652_4d32; // 'FRM2' (docs/PROTOCOL.md)
pub const HEADER_BYTES: usize = 128;

#[repr(C)]
pub struct Header {
    pub magic: u32,
    pub capacity_words: u32,
    /// seqlock: odd while the writer is copying
    pub seq: AtomicU32,
    pub words_used: AtomicU32,
    pub quit: AtomicU32,
    pub last_draw_count: AtomicU32,
    /// packed 0xRRGGBB8 of the last clear colour
    pub last_clear_rgb: AtomicU32,
    pub render_ready: AtomicU32,
    pub frames_published: AtomicU64,
    pub frames_rendered: AtomicU64,
    pub uploads: AtomicU64,
    pub upload_bytes: AtomicU64,
    /// total ns the render thread spent encoding+presenting (for averages)
    pub render_ns: AtomicU64,
    pub drawable_w: AtomicU32,
    pub drawable_h: AtomicU32,
}

const _: () = assert!(std::mem::size_of::<Header>() <= HEADER_BYTES);

pub struct Shm {
    base: *mut u8,
    len: usize,
    sem: *mut libc::sem_t,
    pub name: String,
    pub owner: bool,
}

// SAFETY: the mapping and semaphore are process-wide handles; all shared fields are atomics and the
// payload is guarded by the seqlock.
unsafe impl Send for Shm {}
unsafe impl Sync for Shm {}

fn sem_name(name: &str) -> CString {
    CString::new(format!("{name}-s")).unwrap()
}

impl Shm {
    /// Create (owner, main process). `name` like "/fr-1234" (macOS limits names to 31 chars).
    pub fn create(name: &str, capacity_words: u32) -> Result<Shm, String> {
        let cname = CString::new(name).map_err(|e| e.to_string())?;
        let len = HEADER_BYTES + capacity_words as usize * 4;
        unsafe {
            libc::shm_unlink(cname.as_ptr());
            let fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR | libc::O_EXCL, 0o600);
            if fd < 0 {
                return Err(format!("shm_open(create {name}) failed: errno {}", *errno()));
            }
            if libc::ftruncate(fd, len as libc::off_t) != 0 {
                let e = *errno();
                libc::close(fd);
                return Err(format!("ftruncate failed: errno {e}"));
            }
            let base = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
            libc::close(fd);
            if base == libc::MAP_FAILED {
                return Err(format!("mmap failed: errno {}", *errno()));
            }
            let sn = sem_name(name);
            libc::sem_unlink(sn.as_ptr());
            let sem = libc::sem_open(sn.as_ptr(), libc::O_CREAT | libc::O_EXCL, 0o600 as libc::c_uint, 0 as libc::c_uint);
            if sem == libc::SEM_FAILED {
                return Err(format!("sem_open(create) failed: errno {}", *errno()));
            }
            let h = base as *mut Header;
            (*h).magic = MAGIC_SHM;
            (*h).capacity_words = capacity_words;
            Ok(Shm { base: base as *mut u8, len, sem, name: name.to_string(), owner: true })
        }
    }

    /// Open an existing mailbox (renderer process).
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
            let base = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
            libc::close(fd);
            if base == libc::MAP_FAILED {
                return Err(format!("mmap failed: errno {}", *errno()));
            }
            let h = &*(base as *const Header);
            if h.magic != MAGIC_SHM {
                return Err("shm magic mismatch".into());
            }
            let sn = sem_name(name);
            let sem = libc::sem_open(sn.as_ptr(), 0);
            if sem == libc::SEM_FAILED {
                return Err(format!("sem_open(open) failed: errno {}", *errno()));
            }
            Ok(Shm { base: base as *mut u8, len, sem, name: name.to_string(), owner: false })
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

    /// Writer side: publish `words` (the used prefix of the command buffer) and wake the renderer.
    pub fn publish(&self, words: &[u32]) -> Result<(), String> {
        let h = self.header();
        if words.len() > self.capacity_words() {
            return Err(format!("frame of {} words exceeds mailbox capacity {}", words.len(), self.capacity_words()));
        }
        let s = h.seq.load(Ordering::Relaxed);
        h.seq.store(s.wrapping_add(1) | 1, Ordering::Release); // odd: writing
        std::sync::atomic::fence(Ordering::SeqCst);
        unsafe { std::ptr::copy_nonoverlapping(words.as_ptr(), self.data(), words.len()) };
        h.words_used.store(words.len() as u32, Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::SeqCst);
        h.seq.store((s.wrapping_add(1) | 1).wrapping_add(1), Ordering::Release); // even: stable
        h.frames_published.fetch_add(1, Ordering::Relaxed);
        unsafe { libc::sem_post(self.sem) };
        Ok(())
    }

    /// Reader side: block until a frame is published (or quit). Returns false on quit.
    pub fn wait(&self) -> bool {
        loop {
            let r = unsafe { libc::sem_wait(self.sem) };
            if r == 0 {
                return self.header().quit.load(Ordering::Relaxed) == 0;
            }
            if unsafe { *errno() } != libc::EINTR {
                return false;
            }
        }
    }

    /// Reader side: swallow extra pending wakeups so a slow renderer renders only the newest frame.
    pub fn drain(&self) {
        while unsafe { libc::sem_trywait(self.sem) } == 0 {}
    }

    /// Wake the reader with the quit flag set.
    pub fn request_quit(&self) {
        self.header().quit.store(1, Ordering::Relaxed);
        unsafe { libc::sem_post(self.sem) };
    }

    /// Reader side: copy the latest consistent frame into `out`.
    pub fn snapshot(&self, out: &mut Vec<u32>) {
        let h = self.header();
        loop {
            let s1 = h.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let n = (h.words_used.load(Ordering::Relaxed) as usize).min(self.capacity_words());
            out.clear();
            out.resize(n, 0);
            unsafe { std::ptr::copy_nonoverlapping(self.data(), out.as_mut_ptr(), n) };
            std::sync::atomic::fence(Ordering::SeqCst);
            if h.seq.load(Ordering::Acquire) == s1 {
                return;
            }
        }
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        unsafe {
            libc::sem_close(self.sem);
            libc::munmap(self.base as *mut libc::c_void, self.len);
            if self.owner {
                let c = CString::new(self.name.clone()).unwrap();
                libc::shm_unlink(c.as_ptr());
                libc::sem_unlink(sem_name(&self.name).as_ptr());
            }
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
    if w.len() < 2 || w[0] != MAGIC_FRAME {
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

    fn frame(bg: [f32; 3], draws: u32) -> Vec<u32> {
        let mut v = vec![MAGIC_FRAME, 0, 4, 4, bg[0].to_bits(), bg[1].to_bits(), bg[2].to_bits(), 1f32.to_bits()];
        v.extend([5, 1 + draws * 20, draws]);
        v.extend(std::iter::repeat(0).take((draws * 20) as usize));
        v[1] = v.len() as u32;
        v
    }

    #[test]
    fn parses() {
        let f = parse_frame(&frame([0.25, 0.5, 0.75], 7));
        assert!(f.valid);
        assert_eq!(f.draw_count, 7);
        assert_eq!(f.background, Some([0.25, 0.5, 0.75]));
        assert!(!parse_frame(&[1, 2, 3]).valid);
    }

    #[test]
    fn mailbox_roundtrip_two_handles() {
        let name = format!("/frt-{}", std::process::id());
        let a = Shm::create(&name, 1024).unwrap();
        let b = Shm::open(&name).unwrap(); // same process, separate mapping: like renderer vs main
        let fr = frame([1.0, 0.0, 0.5], 3);
        b.publish(&fr).unwrap();
        assert!(a.wait());
        let mut out = Vec::new();
        a.snapshot(&mut out);
        assert_eq!(out, fr);
        assert_eq!(a.header().frames_published.load(Ordering::Relaxed), 1);
        assert!(b.publish(&vec![0; 2000]).is_err());
        a.request_quit();
        assert!(!a.wait());
    }
}
