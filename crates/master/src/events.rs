//! Master event loop: a single `poll(2)` over the self-pipe (and, in ondemand,
//! the listeners) plus deadline bookkeeping. Drains the self-pipe fully, defers
//! the batched reap until after the drain so control bytes in the same wake
//! update pctl state before corpses are classified, then fires due timers. All
//! policy lives in the pure modules; this file is the executor.

use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::Ordering::{Acquire, Relaxed};
use std::time::{Duration, Instant};

use libc::c_int;
use rapira_scoreboard::{SLOT_ACTIVE, SLOT_FREE, SLOT_IDLE, SLOT_STARTING, Scoreboard, now_millis};

use crate::lifeline::Lifeline;
use crate::pctl::{KillTarget, Pctl, PctlState, ReloadPhase, SignalAction};
use crate::process::{ExitVerdict, ProcTable, WorkerProc, spawn_worker};
use crate::scaling::{DynAction, DynInput, dynamic_start_count, dynamic_tick, ondemand_armed};
use crate::signals::{SIG_CHLD, SelfPipe, errno_get};
use crate::{MasterConfig, PmMode, StopReason, WorkerEnv};

/// While an overlap reload waits for a replacement to report IDLE or ACTIVE,
/// the scoreboard is re-checked on this short cadence. The total wait is bounded
/// by `process_control_timeout` so a broken replacement cannot stall the reload.
const RELOAD_GATE_POLL: Duration = Duration::from_millis(50);

/// Deadline sources merged into one poll timeout: the always-armed 1s tick, the
/// pctl escalation step, and the earliest per-slot respawn.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadlines {
    pub next_tick: Instant,
    pub pctl: Option<Instant>,
    pub respawn: Option<Instant>,
}

impl Deadlines {
    fn new(now: Instant) -> Deadlines {
        Deadlines {
            next_tick: now + Duration::from_secs(1),
            pctl: None,
            respawn: None,
        }
    }

    /// Milliseconds until the earliest deadline, rounded UP so a sub-millisecond
    /// remainder never busy-spins `poll` with a 0 timeout.
    pub fn poll_timeout_ms(&self, now: Instant) -> c_int {
        let mut next = self.next_tick;
        for d in [self.pctl, self.respawn].into_iter().flatten() {
            next = next.min(d);
        }
        let d = next.saturating_duration_since(now);
        let ms = d.as_nanos().div_ceil(1_000_000);
        ms.min(i32::MAX as u128) as c_int
    }
}

fn pollfd(fd: RawFd) -> libc::pollfd {
    libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }
}

/// Drain the nonblocking self-pipe completely, returning bytes in arrival order.
fn drain_pipe(fd: RawFd, buf: &mut [u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        // SAFETY: read into a live buffer from a valid nonblocking fd.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break; // 0 = EOF (never, master holds wr), <0 = EAGAIN
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    out
}

fn kill(pid: libc::pid_t, sig: c_int) {
    // SAFETY: kill is always safe; a stale pid yields ESRCH, harmlessly ignored.
    unsafe { libc::kill(pid, sig) };
}

/// The running master. Owns the worker closure (called only in forked children)
/// and all loop state.
pub(crate) struct Master<F: FnMut(WorkerEnv) -> i32> {
    cfg: MasterConfig,
    scoreboard: Scoreboard,
    self_pipe: SelfPipe,
    lifeline: Lifeline,
    worker: F,
    pctl: Pctl,
    deadlines: Deadlines,
    table: ProcTable,
    spawn_rate: u32,
    /// Ceiling warning latch: warn once per saturation episode, not every tick.
    warned_max_children: bool,
    /// Absolute deadline for the current reload Await gate: once reached, drain
    /// the next old worker even if the replacement never started serving.
    reload_await_until: Option<Instant>,
    /// Latched once the pool has ever served a successful request. Guards the
    /// failboot check: only a pool that never managed to serve is an
    /// unrecoverable boot failure. Needed because scoreboard counters are
    /// unreliable history — a replacement `bind()` zeroes a slot's served count.
    ever_served: bool,
}

impl<F: FnMut(WorkerEnv) -> i32> Master<F> {
    pub(crate) fn new(
        cfg: MasterConfig,
        scoreboard: Scoreboard,
        self_pipe: SelfPipe,
        lifeline: Lifeline,
        worker: F,
    ) -> Master<F> {
        let now = Instant::now();
        let table = ProcTable::new(scoreboard.nslots());
        Master {
            cfg,
            scoreboard,
            self_pipe,
            lifeline,
            worker,
            pctl: Pctl::default(),
            deadlines: Deadlines::new(now),
            table,
            spawn_rate: 1,
            warned_max_children: false,
            reload_await_until: None,
            ever_served: false,
        }
    }

    // ---- scoreboard-derived counts -------------------------------------

    fn idle_count(&self) -> usize {
        self.scoreboard
            .slots()
            .iter()
            .filter(|s| s.state.load(Relaxed) == SLOT_IDLE)
            .count()
    }

    fn starting_count(&self) -> usize {
        self.scoreboard
            .slots()
            .iter()
            .filter(|s| s.state.load(Relaxed) == SLOT_STARTING)
            .count()
    }

    /// Requests that completed WITHOUT error, pool-wide. A failbooting worker
    /// sheds 503s (handled++ AND errors++), so `handled` alone never reaches 0
    /// there — the master must weigh successful service, not raw throughput.
    fn total_successful(&self) -> u64 {
        self.scoreboard
            .slots()
            .iter()
            .map(|s| {
                // Acquire pairs with the worker's Release on `handled` (stored
                // after `errors`): an observed handled implies its error is too,
                // so a shed can never be counted as a success.
                let handled = s.handled.load(Acquire);
                let errors = s.errors.load(Relaxed);
                handled.saturating_sub(errors)
            })
            .sum()
    }

    /// Latch `ever_served` the first time the pool is seen to have served a
    /// successful request. Idempotent; read before the scoreboard can be
    /// overwritten (slot clear / replacement bind).
    fn latch_served(&mut self) {
        if !self.ever_served && self.total_successful() > 0 {
            self.ever_served = true;
        }
    }

    fn slot_is_free(&self, i: usize) -> bool {
        self.scoreboard
            .slot(i)
            .map(|s| s.state.load(Relaxed) == SLOT_FREE)
            .unwrap_or(false)
    }

    fn slot_is_idle(&self, i: usize) -> bool {
        self.scoreboard
            .slot(i)
            .map(|s| s.state.load(Relaxed) == SLOT_IDLE)
            .unwrap_or(false)
    }

    /// The slot's worker is serving: bound and parked (IDLE) or on a request
    /// (ACTIVE). Under load a replacement may never be observed IDLE between
    /// requests, so the reload gate must accept ACTIVE as proof of service.
    fn slot_is_serving(&self, i: usize) -> bool {
        self.scoreboard
            .slot(i)
            .map(|s| {
                let state = s.state.load(Relaxed);
                state == SLOT_IDLE || state == SLOT_ACTIVE
            })
            .unwrap_or(false)
    }

    /// A slot free in the scoreboard, with no live proc and no pending respawn
    /// (so we never race the respawn-deadline path).
    fn find_spawn_slot(&self) -> Option<usize> {
        (0..self.table.slots.len()).find(|&i| {
            self.slot_is_free(i)
                && self.table.slots[i].respawn_at.is_none()
                && !self.table.has_proc(i)
        })
    }

    fn oldest_idle_pid(&self) -> Option<libc::pid_t> {
        self.table
            .procs
            .iter()
            .filter(|p| self.slot_is_idle(p.slot))
            .min_by_key(|p| p.spawned_at)
            .map(|p| p.pid)
    }

    // ---- spawning ------------------------------------------------------

    fn spawn_into(&mut self, slot: usize, now: Instant) {
        self.scoreboard.set_starting(slot);
        let generation = self.table.generation;
        match spawn_worker(
            slot,
            &self.self_pipe,
            &self.lifeline,
            &self.scoreboard,
            &mut self.worker,
        ) {
            Ok(pid) => self.table.procs.push(WorkerProc {
                pid,
                slot,
                generation,
                spawned_at: now,
                idle_kill: false,
            }),
            Err(e) => {
                log::error!(target: "master", "spawn failed for slot {slot}: {e}");
                self.scoreboard.clear(slot);
                // Back off so a readable ondemand listener does not spin.
                self.table.slots[slot].schedule_backoff(Duration::ZERO, now);
            }
        }
    }

    fn fork_initial(&mut self) {
        let now = Instant::now();
        let count = match self.cfg.pm {
            PmMode::Static => self.cfg.processes,
            PmMode::Dynamic {
                min_spare,
                max_spare,
            } => dynamic_start_count(min_spare, max_spare, self.cfg.processes),
            PmMode::Ondemand => 0,
        };
        for _ in 0..count {
            match self.find_spawn_slot() {
                Some(slot) => self.spawn_into(slot, now),
                None => break,
            }
        }
        self.recompute_respawn_deadline();
    }

    // ---- signal handling -----------------------------------------------

    /// Apply a control byte. `Some(reason)` means the loop must return now
    /// (forced stop, or a stop with nothing left to drain).
    fn handle_signal(&mut self, byte: u8, now: Instant) -> Option<StopReason> {
        match self.pctl.on_signal(byte) {
            SignalAction::Stop => self.begin_stop(now),
            SignalAction::Forced => {
                self.force_stop();
                Some(StopReason::Forced)
            }
            SignalAction::Reload => {
                self.begin_reload(now);
                None
            }
            SignalAction::Status => {
                self.log_status();
                None
            }
            SignalAction::Ignore => None,
        }
    }

    fn begin_stop(&mut self, now: Instant) -> Option<StopReason> {
        for p in &self.table.procs {
            kill(p.pid, libc::SIGQUIT);
        }
        for s in &mut self.table.slots {
            s.cancel_respawn();
        }
        self.deadlines.pctl = Some(now + self.cfg.process_control_timeout);
        self.reload_await_until = None; // stop overrides any in-flight reload gate
        if self.table.procs.is_empty() {
            return Some(StopReason::Drained);
        }
        None
    }

    fn force_stop(&mut self) {
        for p in &self.table.procs {
            kill(p.pid, libc::SIGTERM);
        }
    }

    /// Overlap reload: bump the generation and spawn one current-gen worker as
    /// headroom, then wait for it to report IDLE or ACTIVE before any old worker
    /// is drained, so serving capacity never dips below the pool. With no live
    /// old-gen worker there is nothing to overlap — no headroom is spawned.
    fn begin_reload(&mut self, now: Instant) {
        self.table.generation += 1;
        let slot = if self.has_old_gen() {
            self.find_spawn_slot()
        } else {
            None
        };
        self.reload_enter_await(slot, now);
    }

    /// Enter the overlap gate: for non-ondemand spawn a current-gen replacement
    /// into `slot`, arm the short re-check plus the `process_control_timeout`
    /// safety cap, and wait for it to start serving. Ondemand (or no free slot)
    /// spawns nothing and drains the next old worker directly — replacements
    /// come from demand.
    fn reload_enter_await(&mut self, slot: Option<usize>, now: Instant) {
        match slot {
            Some(s) if !matches!(self.cfg.pm, PmMode::Ondemand) => {
                self.spawn_into(s, now);
                self.pctl.set_reload_await(Some(s));
                self.reload_await_until = Some(now + self.cfg.process_control_timeout);
                self.deadlines.pctl = Some(now + RELOAD_GATE_POLL);
            }
            _ => self.reload_quit_next(now),
        }
    }

    /// Overlap-gate probe: once the pending replacement is serving (IDLE or
    /// ACTIVE), QUIT the next old worker. A no-op while still waiting.
    fn reload_try_advance(&mut self, now: Instant) {
        if let PctlState::Reloading(ReloadPhase::Await { slot }) = self.pctl.state {
            let ready = slot.is_none_or(|s| self.slot_is_serving(s));
            if ready {
                self.reload_quit_next(now);
            }
        }
    }

    /// QUIT the next-oldest old-generation worker (→ Drain), or finish the reload
    /// when none remain. Always leaves the Await gate.
    fn reload_quit_next(&mut self, now: Instant) {
        self.reload_await_until = None;
        let cur = self.table.generation;
        let target = self
            .table
            .procs
            .iter()
            .filter(|p| p.generation < cur)
            .min_by_key(|p| p.spawned_at)
            .map(|p| p.pid);
        match target {
            Some(pid) => {
                kill(pid, libc::SIGQUIT);
                self.pctl.set_reload_drain(pid);
                self.deadlines.pctl = Some(now + self.cfg.process_control_timeout);
            }
            None => self.reload_finish(),
        }
    }

    /// Reload complete: back to Normal, clear the pctl deadline and the gate cap.
    fn reload_finish(&mut self) {
        self.pctl.finish_reload();
        self.deadlines.pctl = None;
        self.reload_await_until = None;
    }

    fn has_old_gen(&self) -> bool {
        let cur = self.table.generation;
        self.table.procs.iter().any(|p| p.generation < cur)
    }

    fn log_status(&self) {
        let snap = self.scoreboard.snapshot_slots();
        log::info!(
            target: "master",
            "status: {} running, {} idle, generation {}",
            self.table.running(),
            self.idle_count(),
            self.table.generation
        );
        for s in snap {
            log::info!(
                target: "master",
                "  slot {} pid {} state {} handled {} errors {} recycles {}",
                s.id, s.pid, s.state, s.handled, s.errors, s.recycles
            );
        }
    }

    // ---- child exits ---------------------------------------------------

    fn on_child_exit(
        &mut self,
        w: WorkerProc,
        verdict: ExitVerdict,
        now: Instant,
    ) -> anyhow::Result<()> {
        let slot = w.slot;
        let lived = now.saturating_duration_since(w.spawned_at);
        // Read served history before clearing this slot / handing it to a fork.
        self.latch_served();
        self.scoreboard.clear(slot);

        if let PctlState::Reloading(ReloadPhase::Drain { draining, .. }) = self.pctl.state
            && draining == w.pid
        {
            // Drained old worker reaped. If old workers remain, spawn its
            // replacement into the freed slot and gate on it becoming IDLE
            // before draining the next; the last old worker needs no
            // replacement (begin_reload's headroom already covers it).
            if self.has_old_gen() {
                self.reload_enter_await(Some(slot), now);
            } else {
                self.reload_finish();
            }
            return Ok(());
        }
        if self.pctl.is_stopping() {
            return Ok(()); // draining down; no respawn
        }

        match verdict {
            ExitVerdict::IdleKill => {
                // Trimmed: do not respawn.
            }
            ExitVerdict::Recycle | ExitVerdict::Drain => {
                self.table.slots[slot].schedule_immediate(now);
                self.apply_pm_respawn_gate(slot);
            }
            ExitVerdict::Unhealthy => {
                // Failboot only for a generation-0 worker in a pool that NEVER
                // served a successful request: an unrecoverable boot failure. A
                // reload replacement (gen > 0) dying unhealthy must never take
                // down the running pool — respawn with backoff instead.
                if w.generation == 0 && !self.ever_served {
                    anyhow::bail!(
                        "worker {} exited unhealthy before the pool served any request",
                        w.pid
                    );
                }
                self.table.slots[slot].schedule_backoff(lived, now);
            }
            ExitVerdict::Crash => {
                self.table.slots[slot].schedule_backoff(lived, now);
            }
        }
        Ok(())
    }

    /// Ondemand never proactively respawns a cleanly-exited worker; demand
    /// re-forks. Crash/unhealthy backoff deadlines are kept even under ondemand:
    /// they suppress forking into the slot until they expire (see
    /// `fire_due_deadlines`), throttling a fork-crash loop.
    fn apply_pm_respawn_gate(&mut self, slot: usize) {
        if matches!(self.cfg.pm, PmMode::Ondemand) {
            self.table.slots[slot].cancel_respawn();
        }
    }

    // ---- idle-kill (dynamic trim / ondemand timeout) -------------------

    fn idle_kill_pid(&mut self, pid: libc::pid_t) {
        if let Some(p) = self.table.procs.iter_mut().find(|p| p.pid == pid) {
            if p.idle_kill {
                kill(pid, libc::SIGKILL);
            } else {
                kill(pid, libc::SIGQUIT);
                p.idle_kill = true;
            }
        }
    }

    // ---- periodic maintenance -----------------------------------------

    fn maintenance_tick(&mut self, now: Instant) {
        // Latch served history while workers are live and counters populated,
        // before any exit can clear a slot.
        self.latch_served();
        match self.cfg.pm {
            PmMode::Static => self.static_refill(now),
            PmMode::Dynamic {
                min_spare,
                max_spare,
            } => self.dynamic_maintenance(min_spare, max_spare, now),
            PmMode::Ondemand => self.ondemand_maintenance(now),
        }
    }

    fn static_refill(&mut self, now: Instant) {
        let running = self.table.running();
        let pending = (0..self.table.slots.len())
            .filter(|&i| self.table.slots[i].respawn_at.is_some() && !self.table.has_proc(i))
            .count();
        let committed = running + pending;
        let target = self.cfg.processes;
        for _ in committed..target {
            match self.find_spawn_slot() {
                Some(slot) => self.spawn_into(slot, now),
                None => break,
            }
        }
    }

    fn dynamic_maintenance(&mut self, min_spare: usize, max_spare: usize, now: Instant) {
        let inp = DynInput {
            idle: self.idle_count(),
            running: self.table.running(),
            min_spare,
            max_spare,
            max_children: self.cfg.processes,
        };
        match dynamic_tick(&inp, &mut self.spawn_rate) {
            DynAction::KillOldestIdle => {
                if let Some(pid) = self.oldest_idle_pid() {
                    self.idle_kill_pid(pid);
                }
            }
            DynAction::Spawn(n) => {
                for _ in 0..n {
                    match self.find_spawn_slot() {
                        Some(slot) => self.spawn_into(slot, now),
                        None => break,
                    }
                }
                self.warned_max_children = false;
            }
            DynAction::ReachedMaxChildren => {
                if !self.warned_max_children {
                    self.warned_max_children = true;
                    log::warn!(
                        target: "master",
                        "reached pool.processes ceiling ({}), consider raising it",
                        self.cfg.processes
                    );
                }
            }
            DynAction::Steady => {}
        }
    }

    fn ondemand_maintenance(&mut self, _now: Instant) {
        // Victim = the idle worker with the stalest activity. Selecting by
        // process age instead would let a busy-at-tick older worker shield a
        // long-expired younger one indefinitely.
        let target = self
            .table
            .procs
            .iter()
            .filter(|p| self.slot_is_idle(p.slot))
            .filter_map(|p| {
                self.scoreboard
                    .slot(p.slot)
                    .map(|s| (p.pid, s.last_activity_ms.load(Relaxed)))
            })
            .min_by_key(|&(_, last)| last);
        let Some((pid, last)) = target else {
            return;
        };
        let age_ms = u128::from(now_millis().saturating_sub(last));
        if age_ms >= self.cfg.process_idle_timeout.as_millis() {
            self.idle_kill_pid(pid);
        }
    }

    // ---- deadlines -----------------------------------------------------

    fn escalate(&mut self, now: Instant) {
        if let Some(step) = self.pctl.escalate() {
            match step.target {
                KillTarget::All => {
                    for p in &self.table.procs {
                        kill(p.pid, step.sig);
                    }
                }
                KillTarget::One(pid) => {
                    if self.table.procs.iter().any(|p| p.pid == pid) {
                        kill(pid, step.sig);
                    }
                }
            }
            self.deadlines.pctl = Some(now + Duration::from_secs(1));
        }
    }

    /// The pctl deadline fired. In the reload Await gate it means "re-check the
    /// replacement, and force past one that never accepted once the safety cap
    /// elapses"; otherwise it drives QUIT→TERM→KILL escalation.
    fn on_pctl_deadline(&mut self, now: Instant) {
        let PctlState::Reloading(ReloadPhase::Await { slot }) = self.pctl.state else {
            self.escalate(now);
            return;
        };
        self.reload_try_advance(now);
        // Still waiting: re-arm the short probe, or force past the safety cap.
        if let PctlState::Reloading(ReloadPhase::Await { .. }) = self.pctl.state {
            if self.reload_await_until.is_some_and(|t| now >= t) {
                if let Some(s) = slot {
                    log::warn!(
                        target: "master",
                        "reload: replacement slot {s} not serving within control timeout; proceeding"
                    );
                }
                self.reload_quit_next(now);
            } else {
                self.deadlines.pctl = Some(now + RELOAD_GATE_POLL);
            }
        }
    }

    fn fire_due_deadlines(&mut self, now: Instant) {
        if let Some(t) = self.deadlines.pctl
            && now >= t
        {
            self.on_pctl_deadline(now);
        }
        for slot in 0..self.table.slots.len() {
            if let Some(t) = self.table.slots[slot].respawn_at
                && now >= t
                && !self.table.has_proc(slot)
            {
                self.table.slots[slot].cancel_respawn();
                // Ondemand: the expired backoff only lifts the fork suppression;
                // the next connection forks, never the timer.
                if !matches!(self.cfg.pm, PmMode::Ondemand) {
                    self.spawn_into(slot, now);
                }
            }
        }
        if now >= self.deadlines.next_tick {
            self.deadlines.next_tick = now + Duration::from_secs(1);
            if self.pctl.is_normal() {
                self.maintenance_tick(now);
            }
        }
    }

    fn recompute_respawn_deadline(&mut self) {
        self.deadlines.respawn = self.table.slots.iter().filter_map(|s| s.respawn_at).min();
    }

    fn compute_armed(&self) -> bool {
        if !matches!(self.cfg.pm, PmMode::Ondemand) {
            return false;
        }
        // Arm only when a fork could actually land: with every free slot in
        // crash backoff, a readable level-triggered listener would otherwise
        // busy-spin poll for the whole backoff window.
        ondemand_armed(
            self.pctl.is_normal(),
            self.table.running(),
            self.cfg.processes,
            self.idle_count(),
            self.starting_count(),
        ) && self.find_spawn_slot().is_some()
    }

    fn ondemand_fork_one(&mut self, now: Instant) {
        if let Some(slot) = self.find_spawn_slot() {
            self.spawn_into(slot, now);
        }
    }

    // ---- main loop -----------------------------------------------------

    pub(crate) fn run_loop(&mut self) -> anyhow::Result<StopReason> {
        self.fork_initial();
        loop {
            let mut fds: Vec<libc::pollfd> = Vec::with_capacity(1 + self.cfg.listeners.len());
            fds.push(pollfd(self.self_pipe.rd.as_raw_fd()));
            if self.compute_armed() {
                for &fd in &self.cfg.listeners {
                    fds.push(pollfd(fd));
                }
            }

            let timeout = self.deadlines.poll_timeout_ms(Instant::now());
            // SAFETY: fds is a live slice; timeout is a valid millisecond count.
            let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
            if n < 0 {
                if errno_get() == libc::EINTR {
                    continue; // handler already wrote the byte
                }
                anyhow::bail!("poll: {}", std::io::Error::last_os_error());
            }
            let now = Instant::now();

            // 1. Drain the self-pipe fully; defer the reap until after.
            let mut got_chld = false;
            if n > 0 && (fds[0].revents & libc::POLLIN) != 0 {
                let mut buf = [0u8; 64];
                for b in drain_pipe(self.self_pipe.rd.as_raw_fd(), &mut buf) {
                    if b == SIG_CHLD {
                        got_chld = true;
                    } else if let Some(reason) = self.handle_signal(b, now) {
                        return Ok(reason);
                    }
                }
            }

            // 2. Batched bury + per-exit policy.
            if got_chld {
                for (w, verdict) in self.table.reap_all() {
                    self.on_child_exit(w, verdict, now)?;
                }
                if self.pctl.is_stopping() && self.table.procs.is_empty() {
                    return Ok(StopReason::Drained);
                }
            }

            // 3. Ondemand: a readable listener while armed forks exactly one.
            //    Re-check arming — a control byte above may have changed state.
            if n > 0 && fds.len() > 1 && self.compute_armed() {
                let readable = fds[1..].iter().any(|p| (p.revents & libc::POLLIN) != 0);
                if readable {
                    self.ondemand_fork_one(now);
                }
            }

            // 4. Due timers.
            self.fire_due_deadlines(now);

            // 5. Recompute the earliest respawn for the next poll timeout.
            self.recompute_respawn_deadline();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_timeout_picks_earliest_deadline() {
        let base = Instant::now();
        let d = Deadlines {
            next_tick: base + Duration::from_secs(1),
            pctl: Some(base + Duration::from_millis(250)),
            respawn: Some(base + Duration::from_millis(700)),
        };
        assert_eq!(d.poll_timeout_ms(base), 250);
    }

    #[test]
    fn poll_timeout_ignores_none_deadlines() {
        let base = Instant::now();
        let d = Deadlines {
            next_tick: base + Duration::from_millis(400),
            pctl: None,
            respawn: None,
        };
        assert_eq!(d.poll_timeout_ms(base), 400);
    }

    #[test]
    fn poll_timeout_rounds_up_never_to_zero_before_deadline() {
        let base = Instant::now();
        let at = |next_tick| Deadlines {
            next_tick,
            pctl: None,
            respawn: None,
        };
        // Sub-millisecond remainders round up (a 0 timeout would busy-spin).
        assert_eq!(
            at(base + Duration::from_micros(1500)).poll_timeout_ms(base),
            2
        );
        assert_eq!(at(base + Duration::from_nanos(1)).poll_timeout_ms(base), 1);
        // Exact milliseconds are not rounded; a past deadline yields 0.
        assert_eq!(at(base + Duration::from_millis(3)).poll_timeout_ms(base), 3);
        assert_eq!(at(base).poll_timeout_ms(base + Duration::from_millis(5)), 0);
    }

    // ---- overlap reload gate ------------------------------------------
    //
    // These drive the reload state machine directly on an in-process master:
    // no fork (spawn is never triggered on the paths exercised) and no signal
    // handler install. Sentinel pids sit above PID_MAX_LIMIT, so the harmless
    // QUITs `reload_quit_next` sends resolve to ESRCH.

    use crate::process::WorkerProc;
    use std::os::fd::{FromRawFd, OwnedFd};

    const P_OLD0: libc::pid_t = 2_000_000_001;
    const P_OLD1: libc::pid_t = 2_000_000_002;
    const P_NEW: libc::pid_t = 2_000_000_100;

    fn dummy_self_pipe() -> SelfPipe {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: socketpair fills a 2-element array with two owned fds; no
        // signal handler is installed (test-only, process signal state untouched).
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "socketpair");
        SelfPipe {
            // SAFETY: fds holds two fresh fds we take sole ownership of.
            rd: unsafe { OwnedFd::from_raw_fd(fds[0]) },
            wr: unsafe { OwnedFd::from_raw_fd(fds[1]) },
        }
    }

    fn test_master(nslots: usize, pm: PmMode) -> Master<impl FnMut(WorkerEnv) -> i32> {
        let scoreboard = Scoreboard::create(nslots).unwrap();
        let cfg = MasterConfig {
            processes: 3,
            pm,
            process_idle_timeout: Duration::from_secs(10),
            process_control_timeout: Duration::from_secs(30),
            pidfile: None,
            listeners: Vec::new(),
        };
        Master::new(
            cfg,
            scoreboard,
            dummy_self_pipe(),
            Lifeline::create().unwrap(),
            |_| unreachable!("worker closure must not run in a unit test"),
        )
    }

    fn push_proc<G>(m: &mut Master<G>, pid: libc::pid_t, slot: usize, generation: u32, at: Instant)
    where
        G: FnMut(WorkerEnv) -> i32,
    {
        m.table.procs.push(WorkerProc {
            pid,
            slot,
            generation,
            spawned_at: at,
            idle_kill: false,
        });
    }

    fn set_slot<G>(m: &Master<G>, slot: usize, state: u32)
    where
        G: FnMut(WorkerEnv) -> i32,
    {
        m.scoreboard.slots()[slot].state.store(state, Relaxed);
    }

    #[test]
    fn reload_gate_holds_until_replacement_idle() {
        let mut m = test_master(6, PmMode::Static);
        let t0 = Instant::now();
        m.table.generation = 1; // begin_reload already bumped the generation
        // Three old-gen workers (ascending spawn time), all idle.
        for (i, &pid) in [P_OLD0, P_OLD1, 2_000_000_003].iter().enumerate() {
            push_proc(&mut m, pid, i, 0, t0 + Duration::from_millis(i as u64));
            set_slot(&m, i, SLOT_IDLE);
        }
        // Current-gen replacement in slot 3, not serving yet.
        push_proc(&mut m, P_NEW, 3, 1, t0 + Duration::from_millis(10));
        set_slot(&m, 3, SLOT_STARTING);
        m.pctl.set_reload_await(Some(3));
        m.reload_await_until = Some(t0 + Duration::from_secs(30));

        // Gate closed: replacement still STARTING ⇒ no old worker is drained.
        m.reload_try_advance(t0);
        assert!(matches!(
            m.pctl.state,
            PctlState::Reloading(ReloadPhase::Await { slot: Some(3) })
        ));

        // Replacement reaches IDLE ⇒ gate opens, oldest old worker drained.
        set_slot(&m, 3, SLOT_IDLE);
        m.reload_try_advance(t0);
        assert!(matches!(
            m.pctl.state,
            PctlState::Reloading(ReloadPhase::Drain { draining, .. }) if draining == P_OLD0
        ));
    }

    #[test]
    fn reload_gate_opens_on_active_replacement() {
        // Under load a replacement can be ACTIVE at every probe; the gate must
        // accept that as serving rather than stall to the control timeout.
        let mut m = test_master(6, PmMode::Static);
        let t0 = Instant::now();
        m.table.generation = 1;
        push_proc(&mut m, P_OLD0, 0, 0, t0);
        set_slot(&m, 0, SLOT_IDLE);
        push_proc(&mut m, P_NEW, 3, 1, t0);
        set_slot(&m, 3, SLOT_ACTIVE);
        m.pctl.set_reload_await(Some(3));
        m.reload_await_until = Some(t0 + Duration::from_secs(30));

        m.reload_try_advance(t0);
        assert!(matches!(
            m.pctl.state,
            PctlState::Reloading(ReloadPhase::Drain { draining, .. }) if draining == P_OLD0
        ));
    }

    #[test]
    fn reload_gate_forces_past_stuck_replacement_at_safety_cap() {
        let mut m = test_master(6, PmMode::Static);
        let t0 = Instant::now();
        m.table.generation = 1;
        push_proc(&mut m, P_OLD0, 0, 0, t0);
        set_slot(&m, 0, SLOT_IDLE);
        push_proc(&mut m, P_NEW, 3, 1, t0); // replacement never becomes IDLE
        set_slot(&m, 3, SLOT_STARTING);
        m.pctl.set_reload_await(Some(3));
        // Safety cap already in the past ⇒ the deadline forces the drain.
        m.reload_await_until = Some(t0);
        m.deadlines.pctl = Some(t0);

        m.on_pctl_deadline(t0 + Duration::from_millis(1));
        assert!(matches!(
            m.pctl.state,
            PctlState::Reloading(ReloadPhase::Drain { draining, .. }) if draining == P_OLD0
        ));
    }

    #[test]
    fn reload_gate_rearms_probe_before_safety_cap() {
        let mut m = test_master(6, PmMode::Static);
        let t0 = Instant::now();
        m.table.generation = 1;
        push_proc(&mut m, P_OLD0, 0, 0, t0);
        set_slot(&m, 0, SLOT_IDLE);
        push_proc(&mut m, P_NEW, 3, 1, t0);
        set_slot(&m, 3, SLOT_STARTING); // still booting
        m.pctl.set_reload_await(Some(3));
        m.reload_await_until = Some(t0 + Duration::from_secs(30));

        let now = t0 + Duration::from_millis(1);
        m.on_pctl_deadline(now);
        // Not idle and cap not reached ⇒ stay in Await, re-arm the short probe.
        assert!(matches!(
            m.pctl.state,
            PctlState::Reloading(ReloadPhase::Await { slot: Some(3) })
        ));
        assert_eq!(m.deadlines.pctl, Some(now + RELOAD_GATE_POLL));
    }

    #[test]
    fn reload_finishes_when_last_old_worker_reaped() {
        let mut m = test_master(6, PmMode::Static);
        let t0 = Instant::now();
        m.table.generation = 1;
        // A current-gen worker is already up; the drained old worker has been
        // removed from the table (reaped). No old-gen workers remain.
        push_proc(&mut m, P_NEW, 0, 1, t0);
        let drained = WorkerProc {
            pid: P_OLD0,
            slot: 1,
            generation: 0,
            spawned_at: t0,
            idle_kill: false,
        };
        m.pctl.set_reload_drain(P_OLD0);
        m.deadlines.pctl = Some(t0 + Duration::from_secs(30));

        m.on_child_exit(drained, ExitVerdict::Drain, t0).unwrap();
        assert!(m.pctl.is_normal());
        assert_eq!(m.deadlines.pctl, None);
        assert_eq!(m.reload_await_until, None);
        assert_eq!(m.table.procs.len(), 1); // only the pre-existing current-gen worker
    }

    #[test]
    fn ondemand_reload_paces_one_at_a_time_without_spawning() {
        let mut m = test_master(6, PmMode::Ondemand);
        let t0 = Instant::now();
        for (i, &pid) in [P_OLD0, P_OLD1].iter().enumerate() {
            push_proc(&mut m, pid, i, 0, t0 + Duration::from_millis(i as u64));
            set_slot(&m, i, SLOT_IDLE);
        }

        // Reload: no headroom spawn (ondemand); vacuous gate ⇒ oldest QUIT now.
        m.begin_reload(t0);
        assert_eq!(m.table.generation, 1);
        assert_eq!(
            m.table.procs.len(),
            2,
            "ondemand must not spawn a replacement"
        );
        assert!(matches!(
            m.pctl.state,
            PctlState::Reloading(ReloadPhase::Drain { draining, .. }) if draining == P_OLD0
        ));

        // First old worker reaped ⇒ next oldest QUIT, still no spawn.
        let i = m.table.procs.iter().position(|p| p.pid == P_OLD0).unwrap();
        let w0 = m.table.procs.swap_remove(i);
        m.on_child_exit(w0, ExitVerdict::Drain, t0).unwrap();
        assert_eq!(m.table.procs.len(), 1);
        assert!(matches!(
            m.pctl.state,
            PctlState::Reloading(ReloadPhase::Drain { draining, .. }) if draining == P_OLD1
        ));

        // Last old worker reaped ⇒ reload complete.
        let i = m.table.procs.iter().position(|p| p.pid == P_OLD1).unwrap();
        let w1 = m.table.procs.swap_remove(i);
        m.on_child_exit(w1, ExitVerdict::Drain, t0).unwrap();
        assert!(m.pctl.is_normal());
        assert_eq!(m.table.procs.len(), 0);
    }

    // ---- failboot guard (ever_served latch) ---------------------------

    fn dead_worker(pid: libc::pid_t, slot: usize, generation: u32, at: Instant) -> WorkerProc {
        WorkerProc {
            pid,
            slot,
            generation,
            spawned_at: at,
            idle_kill: false,
        }
    }

    #[test]
    fn unhealthy_after_ever_served_respawns_not_failboot() {
        let mut m = test_master(3, PmMode::Static);
        let t0 = Instant::now();
        m.ever_served = true; // the pool has already served successfully
        let w = dead_worker(4242, 0, 0, t0);
        // Recoverable: unhealthy death must schedule a backoff respawn, not bail.
        let r = m.on_child_exit(w, ExitVerdict::Unhealthy, t0 + Duration::from_secs(1));
        assert!(r.is_ok());
        assert!(m.table.slots[0].respawn_at.is_some());
    }

    #[test]
    fn gen0_unhealthy_never_served_failboots() {
        let mut m = test_master(3, PmMode::Static);
        let t0 = Instant::now();
        assert!(!m.ever_served);
        // Fresh pool, nothing served → unrecoverable boot failure → Err.
        let w = dead_worker(4243, 0, 0, t0);
        let r = m.on_child_exit(w, ExitVerdict::Unhealthy, t0);
        assert!(r.is_err());
    }

    #[test]
    fn gen1_unhealthy_never_served_respawns_not_failboot() {
        // A reload replacement dying unhealthy before the pool served anything
        // must back off and respawn, never kill the healthy old generation.
        let mut m = test_master(3, PmMode::Static);
        let t0 = Instant::now();
        m.table.generation = 1;
        assert!(!m.ever_served);
        let w = dead_worker(4244, 0, 1, t0);
        let r = m.on_child_exit(w, ExitVerdict::Unhealthy, t0);
        assert!(r.is_ok());
        assert!(m.table.slots[0].respawn_at.is_some());
    }

    #[test]
    fn ondemand_crash_backoff_suppresses_without_respawn() {
        // A crash under ondemand keeps its backoff deadline as fork suppression:
        // the slot is not spawnable (loop stays disarmed) until the deadline,
        // whose expiry only lifts the suppression — it never forks.
        let mut m = test_master(2, PmMode::Ondemand);
        let t0 = Instant::now();
        let w = dead_worker(4245, 0, 0, t0);
        m.on_child_exit(w, ExitVerdict::Crash, t0 + Duration::from_secs(1))
            .unwrap();
        let due = m.table.slots[0]
            .respawn_at
            .expect("backoff kept as suppression");

        m.fire_due_deadlines(due + Duration::from_millis(1));
        assert_eq!(m.table.slots[0].respawn_at, None, "suppression lifted");
        assert!(m.table.procs.is_empty(), "expiry must not fork");
    }
}
