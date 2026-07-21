//! Shared-memory scoreboard: one cache-line slot per worker process, created by
//! the master via anonymous shared mmap before any fork and inherited by every
//! worker. Workers write their own slot; the master reads all slots for pm
//! decisions and writes only STARTING (at fork) and FREE (after reap).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::time::{SystemTime, UNIX_EPOCH};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub const SB_MAGIC: u32 = u32::from_le_bytes(*b"RPSB");
pub const SB_VERSION: u32 = 1;
pub const SB_MAX_SLOTS: usize = 4096;

pub const SLOT_FREE: u32 = 0; // no worker bound (master clears after reap)
pub const SLOT_STARTING: u32 = 1; // master forked; worker has not reported in yet
pub const SLOT_IDLE: u32 = 2; // parked waiting for a job — spare capacity
pub const SLOT_ACTIVE: u32 = 3; // executing a request
pub const SLOT_DRAINING: u32 = 4; // self-initiated exit pending (quota / unhealthy)

/// Layout oracle for [`SharedSlot`]: same fields as plain ints. The atomics
/// variant is layout-compatible (atomics have the same size/align as their
/// underlying integers), but lacks zerocopy's `Immutable`, so the derives and
/// const asserts live here.
#[repr(C, align(64))]
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
pub struct SlotRaw {
    pub state: u32,
    pub pid: u32,
    pub handled: u64,
    pub errors: u64,
    pub recycles: u64,
    pub restarts: u64,
    pub unhealthy: u32,
    _pad: [u8; 4],
    pub last_activity_ms: u64,
    _tail: [u8; 8],
}

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

#[repr(C, align(64))]
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
struct ShmHeader {
    magic: u32,
    version: u32,
    nslots: u32,
    _pad: [u8; 52],
}

const _: () = assert!(size_of::<SlotRaw>() == 64 && align_of::<SlotRaw>() == 64);
const _: () = assert!(size_of::<SharedSlot>() == size_of::<SlotRaw>());
const _: () = assert!(align_of::<SharedSlot>() == align_of::<SlotRaw>());
const _: () = assert!(size_of::<ShmHeader>() == 64);

/// Copy view over the mapping; the addresses are identical in every forked
/// child because the mmap happens once, pre-fork.
#[derive(Clone, Copy)]
pub struct Scoreboard {
    header: &'static ShmHeader,
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

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Scoreboard {
    /// Create the shared mapping. Master-side, pre-fork; also the in-process
    /// path with `nslots = 1` (tests, fused single-process boot).
    pub fn create(nslots: usize) -> anyhow::Result<Scoreboard> {
        anyhow::ensure!(
            (1..=SB_MAX_SLOTS).contains(&nslots),
            "scoreboard slots out of range: {nslots}"
        );
        let bytes = size_of::<ShmHeader>() + nslots * size_of::<SharedSlot>();
        // SAFETY: the single mmap->&'static cast in the codebase.
        //  * MAP_SHARED|MAP_ANONYMOUS is page-aligned (>= 64) and zero-filled; zero
        //    is a valid bit pattern for every field (FromBytes on the layout oracle).
        //  * No implicit padding (IntoBytes derives + const size asserts above).
        //  * The mapping is never munmap'd -> lives for the process and every
        //    fork -> 'static.
        //  * All post-publication mutation goes through atomics; the plain-int
        //    header is fully written below before any reference escapes.
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
            let hdr = ptr.cast::<ShmHeader>();
            (*hdr).magic = SB_MAGIC;
            (*hdr).version = SB_VERSION;
            (*hdr).nslots = nslots as u32;
            let slots = std::slice::from_raw_parts(
                ptr.cast::<u8>()
                    .add(size_of::<ShmHeader>())
                    .cast::<SharedSlot>(),
                nslots,
            );
            Ok(Scoreboard {
                header: &*hdr,
                slots,
            })
        }
    }

    pub fn nslots(&self) -> usize {
        self.header.nslots as usize
    }

    /// Sanity check for a mapping inherited across fork.
    pub fn header_valid(&self) -> bool {
        self.header.magic == SB_MAGIC && self.header.version == SB_VERSION
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

    /// Find a FREE slot index (master-side; single-threaded, so no CAS races).
    pub fn find_free(&self) -> Option<usize> {
        self.slots
            .iter()
            .position(|s| s.state.load(Relaxed) == SLOT_FREE)
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
        assert_eq!(sb.find_free(), Some(0));

        sb.set_starting(0);
        assert_eq!(sb.find_free(), Some(1));

        let slot = sb.slot(0).unwrap();
        slot.bind(4242);
        slot.handled.fetch_add(2, Relaxed);
        slot.errors.fetch_add(1, Relaxed);

        let snap = sb.snapshot_slots();
        assert_eq!(snap.len(), 1);
        assert_eq!((snap[0].pid, snap[0].handled, snap[0].errors), (4242, 2, 1));
        assert_eq!(snap[0].state, SLOT_IDLE);

        sb.clear(0);
        assert_eq!(sb.find_free(), Some(0));
        assert!(sb.snapshot_slots().is_empty());
    }

    #[test]
    fn slots_out_of_range_rejected() {
        assert!(Scoreboard::create(0).is_err());
        assert!(Scoreboard::create(SB_MAX_SLOTS + 1).is_err());
    }
}
