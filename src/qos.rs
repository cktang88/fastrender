//! Thread quality-of-service classes (macOS; a no-op elsewhere).
//!
//! On a 4P+4E Apple-silicon Mac the scheduler places threads by QoS class: an unclassified `std::thread` is
//! `DEFAULT` and can be parked on an efficiency core (~2.5-3x slower), which shows up as frame-time spikes rather
//! than mean time (docs/research/perf-audit-2026-11.md T4, docs/research/metal-2026-optimization.md section 3).
//!
//! Who sets what:
//! - the thread that renders (`mac::Renderer::new` is called on it: the main thread of `fastrender run/bench`, the
//!   addon's `fastrender-render` thread in Electron): `UserInteractive`;
//! - the Electron ring reader (decodes the frame the render thread waits for): `UserInitiated`;
//! - compile / translate workers: `UserInitiated` when a frame waits on them, `Default` for prewarming;
//! - warm-up and other background work: `Utility`.
//!
//! No thread here spins: back-pressure blocks on a condition variable / `MTLSharedEvent` (`mac::FrameFence`), the
//! display link or `nextDrawable` paces the windowed loop, the Electron reader blocks on a POSIX semaphore.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Qos {
    /// work the user is looking at this frame: the render thread, the game's JS thread
    UserInteractive,
    /// work a frame is waiting for
    UserInitiated,
    /// prewarming, streaming
    Default,
    /// long-running background work (translator warm-up, cache writes)
    Utility,
    Background,
}

impl Qos {
    /// `qos_class_t` (sys/qos.h)
    pub fn class(self) -> u32 {
        match self {
            Qos::UserInteractive => 0x21,
            Qos::UserInitiated => 0x19,
            Qos::Default => 0x15,
            Qos::Utility => 0x11,
            Qos::Background => 0x09,
        }
    }

    pub fn from_class(c: u32) -> Option<Qos> {
        [Qos::UserInteractive, Qos::UserInitiated, Qos::Default, Qos::Utility, Qos::Background].into_iter().find(|q| q.class() == c)
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
    fn qos_class_self() -> u32;
}

/// Set the calling thread's QoS class. Returns false when the kernel refuses (a thread that opted out of QoS by
/// setting an explicit scheduling policy) and always on other systems.
pub fn set_thread_qos(q: Qos) -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { pthread_set_qos_class_self_np(q.class(), 0) == 0 }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = q;
        false
    }
}

/// The calling thread's QoS class, None when unspecified (or not macOS).
pub fn thread_qos() -> Option<Qos> {
    #[cfg(target_os = "macos")]
    {
        Qos::from_class(unsafe { qos_class_self() })
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_round_trip_and_are_ordered() {
        let all = [Qos::UserInteractive, Qos::UserInitiated, Qos::Default, Qos::Utility, Qos::Background];
        for q in all {
            assert_eq!(Qos::from_class(q.class()), Some(q));
        }
        assert!(all.windows(2).all(|w| w[0].class() > w[1].class()));
        assert_eq!(Qos::from_class(0), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_thread_takes_the_class_it_asks_for() {
        std::thread::spawn(|| {
            assert!(set_thread_qos(Qos::UserInteractive));
            assert_eq!(thread_qos(), Some(Qos::UserInteractive));
            assert!(set_thread_qos(Qos::Utility));
            assert_eq!(thread_qos(), Some(Qos::Utility));
        })
        .join()
        .unwrap();
    }
}
