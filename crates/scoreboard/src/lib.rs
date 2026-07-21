//! Shared-memory scoreboard: one cache-line slot per worker process, created by
//! the master via anonymous shared mmap before any fork and inherited by every
//! worker. Workers write their own slot; the master reads all slots for pm
//! decisions and writes only STARTING (at fork) and FREE (after reap).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

pub const SB_MAX_SLOTS: usize = 4096;

pub const SLOT_FREE: u32 = 0; // no worker bound (master clears after reap)
pub const SLOT_STARTING: u32 = 1; // master forked; worker has not reported in yet
pub const SLOT_IDLE: u32 = 2; // parked waiting for a job — spare capacity
pub const SLOT_ACTIVE: u32 = 3; // executing a request
pub const SLOT_DRAINING: u32 = 4; // self-initiated exit pending (quota / unhealthy)

/// The live view over one mapped slot. Single-writer (its worker); the master
/// only reads it, except for the STARTING/FREE ownership transitions.
#[repr(C, align(64))]
pub struct SharedSlot {
    pub state: AtomicU32,
    pub pid: AtomicU32,
    pub handled: AtomicU64,
    pub errors: AtomicU64,
    pub recycles: AtomicU64,
    pub restarts: AtomicU64,
    pub unhealthy: AtomicU32,
    _pad: [u8; 4],
    pub last_activity_ms: AtomicU64,
    _tail: [u8; 8],
}

const _: () = assert!(size_of::<SharedSlot>() == 64 && align_of::<SharedSlot>() == 64);

/// Copy view over the mapping; the addresses are identical in every forked
/// child because the mmap happens once, pre-fork.
#[derive(Clone, Copy)]
pub struct Scoreboard {
    slots: &'static [SharedSlot],
}

#[derive(Debug, Default, Clone)]
pub struct SlotSnapshot {
    pub id: usize,
    pub pid: u32,
    pub state: u32,
    pub handled: u64,
    pub errors: u64,
    pub recycles: u64,
    pub restarts: u64,
    pub unhealthy: bool,
    pub last_activity_ms: u64,
}

/// Milliseconds on `CLOCK_MONOTONIC`: cross-process comparable within one boot
/// and immune to wall-clock steps that would fake request/idle ages.
pub fn now_millis() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts is a live out-param; CLOCK_MONOTONIC always exists.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

impl Scoreboard {
    /// Create the shared mapping. Master-side, pre-fork; also the in-process
    /// path with `nslots = 1` (tests, fused single-process boot).
    pub fn create(nslots: usize) -> anyhow::Result<Scoreboard> {
        anyhow::ensure!(
            (1..=SB_MAX_SLOTS).contains(&nslots),
            "scoreboard slots out of range: {nslots}"
        );
        let bytes = nslots * size_of::<SharedSlot>();
        // SAFETY: the single mmap->&'static cast in the codebase.
        //  * MAP_SHARED|MAP_ANONYMOUS is page-aligned (>= 64) and zero-filled; zero
        //    is a valid bit pattern for every field (atomic ints and u8 arrays).
        //  * No implicit padding (field sizes sum to 64; const assert above).
        //  * The mapping is never munmap'd -> lives for the process and every
        //    fork -> 'static.
        //  * All post-publication mutation goes through atomics.
        unsafe {
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            anyhow::ensure!(
                ptr != libc::MAP_FAILED,
                "scoreboard mmap failed: {}",
                std::io::Error::last_os_error()
            );
            let slots = std::slice::from_raw_parts(ptr.cast::<SharedSlot>(), nslots);
            Ok(Scoreboard { slots })
        }
    }

    pub fn nslots(&self) -> usize {
        self.slots.len()
    }

    pub fn slot(&self, i: usize) -> Option<&'static SharedSlot> {
        self.slots.get(i)
    }

    pub fn slots(&self) -> &'static [SharedSlot] {
        self.slots
    }

    /// Master-side, at fork time: reserve the slot so pm=ondemand suppression
    /// and spare-capacity math see the in-flight fork.
    pub fn set_starting(&self, i: usize) {
        if let Some(s) = self.slots.get(i) {
            s.state.store(SLOT_STARTING, Relaxed);
            s.last_activity_ms.store(now_millis(), Relaxed);
        }
    }

    /// Master-side, after reaping the slot's worker; the slot may be handed to
    /// a new fork afterwards.
    pub fn clear(&self, i: usize) {
        if let Some(s) = self.slots.get(i) {
            s.pid.store(0, Relaxed);
            s.state.store(SLOT_FREE, Relaxed);
        }
    }

    pub fn snapshot_slots(&self) -> Vec<SlotSnapshot> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.state.load(Relaxed) != SLOT_FREE || s.pid.load(Relaxed) != 0)
            .map(|(id, s)| SlotSnapshot {
                id,
                pid: s.pid.load(Relaxed),
                state: s.state.load(Relaxed),
                handled: s.handled.load(Relaxed),
                errors: s.errors.load(Relaxed),
                recycles: s.recycles.load(Relaxed),
                restarts: s.restarts.load(Relaxed),
                unhealthy: s.unhealthy.load(Relaxed) != 0,
                last_activity_ms: s.last_activity_ms.load(Relaxed),
            })
            .collect()
    }
}

impl SharedSlot {
    /// Claim + reset. Exactly once per worker process, before requests flow.
    pub fn bind(&'static self, pid: u32) {
        self.handled.store(0, Relaxed);
        self.errors.store(0, Relaxed);
        self.recycles.store(0, Relaxed);
        self.restarts.store(0, Relaxed);
        self.unhealthy.store(0, Relaxed);
        self.pid.store(pid, Relaxed);
        self.last_activity_ms.store(now_millis(), Relaxed);
        self.state.store(SLOT_IDLE, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_bind_snapshot_roundtrip() {
        let sb = Scoreboard::create(3).unwrap();
        assert_eq!(sb.nslots(), 3);
        assert_eq!(sb.slot(0).unwrap().state.load(Relaxed), SLOT_FREE);

        sb.set_starting(0);
        assert_eq!(sb.slot(0).unwrap().state.load(Relaxed), SLOT_STARTING);
        assert_eq!(sb.slot(1).unwrap().state.load(Relaxed), SLOT_FREE);

        let slot = sb.slot(0).unwrap();
        slot.bind(4242);
        slot.handled.fetch_add(2, Relaxed);
        slot.errors.fetch_add(1, Relaxed);

        let snap = sb.snapshot_slots();
        assert_eq!(snap.len(), 1);
        assert_eq!((snap[0].pid, snap[0].handled, snap[0].errors), (4242, 2, 1));
        assert_eq!(snap[0].state, SLOT_IDLE);

        sb.clear(0);
        assert_eq!(sb.slot(0).unwrap().state.load(Relaxed), SLOT_FREE);
        assert!(sb.snapshot_slots().is_empty());
    }

    #[test]
    fn slots_out_of_range_rejected() {
        assert!(Scoreboard::create(0).is_err());
        assert!(Scoreboard::create(SB_MAX_SLOTS + 1).is_err());
    }
}
