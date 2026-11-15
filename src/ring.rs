//! The v3 per-frame-slot ring (src/mac_three.rs): one shared CPU-visible buffer per frame slot holding this frame's
//! uniform blocks, FrFrame / FrPass records, shadow-pass records and FrObjects at 256-byte-aligned offsets.
//!
//! Portable (no Metal) so the layout and the bounds checks are unit-tested on every OS:
//! - [`RingLayout`] computes every region's offset and the total size a frame needs, from the frame's counts, once
//!   per frame (the slot's buffer is grown to `need` BEFORE anything is written or encoded);
//! - [`Bump`] hands out offsets inside one region and refuses to go past its end;
//! - [`RingWriter`] writes plain-old-data at an offset only if it fits the buffer's capacity. A write that does not
//!   fit is a bug in the sizing above: it trips a debug assertion, and in release it is skipped and counted (a draw
//!   missing for a frame instead of a store past the buffer, which is what crashed the real game: run 43698589636).

/// Metal wants constant-buffer offsets 256-byte aligned on Mac-family GPUs.
pub const RING_ALIGN: usize = 256;

pub const fn align(n: usize) -> usize {
    n.div_ceil(RING_ALIGN) * RING_ALIGN
}

/// Region offsets of one frame in the ring. Order: uniform blocks, main FrFrame + FrPass, the shadow area (FrFrame +
/// FrPass per 2D map or cube face, a FrPass and two blocks per VSM blur), the main pass's FrObjects (one per draw),
/// the shadow passes' FrObjects (one per draw, identity view).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RingLayout {
    pub frame: usize,
    pub pass: usize,
    pub shadow_area: usize,
    pub shadow_area_end: usize,
    pub objects: usize,
    pub shadow_objects: usize,
    /// bytes the whole frame needs
    pub need: usize,
}

impl RingLayout {
    /// `blocks`: bytes of the (already aligned) uniform blocks; `faces`: shadow passes (2D maps + 6 per cube);
    /// `shadows`: SHADOWS entries (each may add a VSM blur: a FrPass + 2 blocks of `vsm_block` bytes);
    /// `draws`: draw records; `object`: size of one FrObject.
    pub fn new(blocks: usize, faces: usize, shadows: usize, vsm_block: usize, draws: usize, object: usize) -> Self {
        let frame = align(blocks);
        let pass = frame + RING_ALIGN;
        let shadow_area = pass + RING_ALIGN;
        let shadow_area_end = shadow_area + (2 * faces + shadows) * RING_ALIGN + shadows * 2 * align(vsm_block);
        let objects = shadow_area_end;
        let shadow_objects = objects + draws * align(object);
        let need = shadow_objects + draws * align(object);
        RingLayout { frame, pass, shadow_area, shadow_area_end, objects, shadow_objects, need }
    }

    /// The ring capacity to allocate for `need` bytes (powers of two: a growing scene re-allocates rarely).
    pub fn capacity_for(need: usize) -> usize {
        need.max(64 << 10).next_power_of_two()
    }
}

/// Offsets handed out inside one region [at, end).
#[derive(Debug, Clone, Copy)]
pub struct Bump {
    at: usize,
    end: usize,
}

impl Bump {
    pub fn new(start: usize, end: usize) -> Self {
        Bump { at: start, end }
    }
    /// The next `n` bytes (rounded up to RING_ALIGN), or None when the region is full.
    pub fn take(&mut self, n: usize) -> Option<usize> {
        let o = self.at;
        let next = o.checked_add(align(n))?;
        if next > self.end {
            debug_assert!(false, "ring region overflow: {o} + {n} > {}", self.end);
            return None;
        }
        self.at = next;
        Some(o)
    }
}

/// Bounds-checked writes into a ring buffer's mapped memory.
#[derive(Clone, Copy)]
pub struct RingWriter {
    base: *mut u8,
    cap: usize,
}

impl RingWriter {
    /// # Safety
    /// `base` must point to at least `cap` writable bytes that stay valid while the writer is used.
    pub unsafe fn new(base: *mut u8, cap: usize) -> Self {
        RingWriter { base, cap }
    }

    pub fn fits(&self, off: usize, len: usize) -> bool {
        off.checked_add(len).is_some_and(|e| e <= self.cap)
    }

    fn check(&self, off: usize, len: usize) -> bool {
        let ok = self.fits(off, len);
        debug_assert!(ok, "ring write past capacity: {off} + {len} > {}", self.cap);
        if !ok {
            OVERFLOWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        ok
    }

    /// Writes `v` at `off`; false (nothing written) when it does not fit.
    pub fn put<T: Copy>(&self, off: usize, v: T) -> bool {
        if !self.check(off, std::mem::size_of::<T>()) {
            return false;
        }
        unsafe { (self.base.add(off) as *mut T).write_unaligned(v) };
        true
    }

    pub fn bytes(&self, off: usize, src: &[u8]) -> bool {
        if !self.check(off, src.len()) {
            return false;
        }
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), self.base.add(off), src.len()) };
        true
    }

    pub fn zero(&self, off: usize, len: usize) -> bool {
        if !self.check(off, len) {
            return false;
        }
        unsafe { std::ptr::write_bytes(self.base.add(off), 0, len) };
        true
    }
}

/// Writes refused since start (release builds; debug builds assert). Reported by the bench.
pub static OVERFLOWS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_regions_are_ordered_aligned_and_sized() {
        // the real game's shape: 277 blocks, CSM (4 cascades) + a point light, 2826 draws, FrObject = 176 bytes
        let blocks = 277 * 512 + 100;
        let l = RingLayout::new(blocks, 4 + 6, 5, 512, 2826, 176);
        for o in [l.frame, l.pass, l.shadow_area, l.objects, l.shadow_objects, l.need] {
            assert_eq!(o % RING_ALIGN, 0);
        }
        assert!(l.frame >= blocks && l.pass > l.frame && l.shadow_area > l.pass);
        assert_eq!(l.shadow_area_end, l.shadow_area + (2 * 10 + 5) * RING_ALIGN + 5 * 2 * 512);
        assert_eq!(l.shadow_objects - l.objects, 2826 * 256);
        assert_eq!(l.need - l.shadow_objects, 2826 * 256);
        assert!(RingLayout::capacity_for(l.need) >= l.need);
        assert_eq!(RingLayout::capacity_for(1), 64 << 10);
    }

    #[test]
    fn every_shadow_record_fits_its_region() {
        // per face a FrFrame + FrPass, per VSM entry a FrPass + 2 blocks: exactly what prepare_shadows_v3 takes
        for (faces, shadows) in [(0, 0), (1, 1), (6, 1), (4 + 6 * 3, 7), (31 * 6, 31)] {
            let l = RingLayout::new(0, faces, shadows, 512, 0, 176);
            let mut b = Bump::new(l.shadow_area, l.shadow_area_end);
            for _ in 0..faces {
                assert!(b.take(RING_ALIGN).is_some() && b.take(RING_ALIGN).is_some());
            }
            for _ in 0..shadows {
                assert!(b.take(RING_ALIGN).is_some() && b.take(512).is_some() && b.take(512).is_some());
            }
        }
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn bump_refuses_past_its_region() {
        let mut b = Bump::new(256, 1024);
        assert_eq!(b.take(1), Some(256));
        assert_eq!(b.take(256), Some(512));
        assert_eq!(b.take(300), None); // 768 + 512 > 1024
        assert_eq!(b.take(256), Some(768));
        assert_eq!(b.take(1), None);
    }

    #[test]
    #[cfg(not(debug_assertions))]
    fn writer_refuses_past_capacity() {
        let mut mem = vec![0u8; 1024 + 64];
        let w = unsafe { RingWriter::new(mem.as_mut_ptr(), 1024) };
        let before = OVERFLOWS.load(std::sync::atomic::Ordering::Relaxed);
        assert!(w.put(1024 - 16, [1.0f32; 4]));
        assert!(!w.put(1024 - 8, [2.0f32; 4])); // the 32-byte-store crash shape: refused
        assert!(!w.put(usize::MAX - 4, 0u32));
        assert!(!w.bytes(1000, &[7u8; 25]));
        assert!(w.zero(900, 24));
        assert!(OVERFLOWS.load(std::sync::atomic::Ordering::Relaxed) >= before + 3);
        assert!(mem[1024..].iter().all(|&b| b == 0), "nothing written past capacity");
        assert_eq!(&mem[1024 - 16..1024 - 12], &1.0f32.to_ne_bytes());
    }

    #[test]
    #[should_panic(expected = "ring write past capacity")]
    #[cfg(debug_assertions)]
    fn writer_asserts_in_debug() {
        let mut mem = vec![0u8; 64];
        let w = unsafe { RingWriter::new(mem.as_mut_ptr(), 64) };
        w.put(48, [0f32; 8]);
    }
}
