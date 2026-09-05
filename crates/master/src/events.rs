use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::Ordering::{Acquire, Relaxed};
use std::time::{Duration, Instant};

use libc::c_int;
use rapira_scoreboard::{SLOT_ACTIVE, SLOT_FREE, SLOT_IDLE, SLOT_STARTING, Scoreboard, now_millis};

use crate::lifeline::Lifeline;
use crate::pctl::{KillTarget, Pctl, PctlState, ReloadPhase, SignalAction};
use crate::process::{ExitVerdict, KillIntent, ProcTable, WorkerProc, spawn_worker};
use crate::scaling::{DynAction, DynInput, dynamic_start_count, dynamic_tick, ondemand_armed};
use crate::signals::{SIG_CHLD, SelfPipe, errno_get};
use crate::{MasterConfig, Scaling, StopReason, WorkerEnv};

/// Re-check cadence for the overlap reload gate; the total wait is bounded by `process_control_timeout`.
const RELOAD_GATE_POLL: Duration = Duration::from_millis(50);

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

    /// Milliseconds until the earliest deadline, rounded up so a sub-millisecond remainder never busy-spins `poll` with a 0 timeout.
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

fn drain_pipe(fd: RawFd, buf: &mut [u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        // SAFETY: read into a live buffer from a valid nonblocking fd.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    out
}

fn kill(pid: libc::pid_t, sig: c_int) {
    // SAFETY: kill is always safe; a stale pid yields ESRCH, harmlessly ignored.
    unsafe { libc::kill(pid, sig) };
}

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
    warned_max_children: bool,
    /// Absolute cap on the reload Await gate: once reached, drain the next old worker anyway.
    reload_await_until: Option<Instant>,
    /// Latched history: scoreboard counters cannot carry it, a replacement `bind()` zeroes a slot's served count.
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

    /// Requests completed without error: Acquire pairs with the worker's Release on `handled` (stored after `errors`) so a shed 503 never counts as a success.
    fn total_successful(&self) -> u64 {
        self.scoreboard
            .slots()
            .iter()
            .map(|s| {
                let handled = s.handled.load(Acquire);
                let errors = s.errors.load(Relaxed);
                handled.saturating_sub(errors)
            })
            .sum()
    }

    /// Must run before a slot clear or a replacement bind can overwrite the scoreboard counters.
    fn latch_served(&mut self) {
        if !self.ever_served && self.total_successful() > 0 {
            self.ever_served = true;
        }
    }

    fn slot_is_free(&self, i: usize) -> bool {
        self.scoreboard.slot(i).state.load(Relaxed) == SLOT_FREE
    }

    // Acquire: ondemand maintenance pairs this with a later timestamp read
    fn slot_is_idle(&self, i: usize) -> bool {
        self.scoreboard.slot(i).state.load(Acquire) == SLOT_IDLE
    }

    /// Serving is IDLE or ACTIVE: under load a replacement may never be observed IDLE between requests.
    fn slot_is_serving(&self, i: usize) -> bool {
        let state = self.scoreboard.slot(i).state.load(Relaxed);
        state == SLOT_IDLE || state == SLOT_ACTIVE
    }

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
                kill_intent: None,
            }),
            Err(e) => {
                tracing::error!(target: "master", "spawn failed for slot {slot}: {e}");
                self.scoreboard.clear(slot);
                self.table.slots[slot].schedule_backoff(Duration::ZERO, now);
            }
        }
    }

    fn fork_initial(&mut self) {
        let now: Instant = Instant::now();
        let count: usize = match self.cfg.scaling {
            Scaling::Static => self.cfg.processes,
            Scaling::Dynamic {
                min_spare,
                max_spare,
            } => dynamic_start_count(min_spare, max_spare, self.cfg.processes),
            Scaling::Ondemand => 0,
        };
        for _ in 0..count {
            match self.find_spawn_slot() {
                Some(slot) => self.spawn_into(slot, now),
                None => break,
            }
        }
        self.recompute_respawn_deadline();
    }

    /// `Some(reason)` means the loop must return now: forced stop, or a stop with nothing left to drain.
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
        self.reload_await_until = None;
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

    /// Overlap reload: spawn one current-gen worker as headroom and gate on it serving before any old worker is drained, so capacity never dips.
    fn begin_reload(&mut self, now: Instant) {
        self.table.generation += 1;
        let slot = if self.has_old_gen() {
            self.find_spawn_slot()
        } else {
            None
        };
        self.reload_enter_await(slot, now);
    }

    /// Ondemand (or no free slot) spawns no replacement and drains the next old worker directly: replacements come from demand.
    fn reload_enter_await(&mut self, slot: Option<usize>, now: Instant) {
        match slot {
            Some(s) if !matches!(self.cfg.scaling, Scaling::Ondemand) => {
                self.spawn_into(s, now);
                self.pctl.set_reload_await(Some(s));
                self.reload_await_until = Some(now + self.cfg.process_control_timeout);
                self.deadlines.pctl = Some(now + RELOAD_GATE_POLL);
            }
            _ => self.reload_quit_next(now),
        }
    }

    fn reload_try_advance(&mut self, now: Instant) {
        if let PctlState::Reloading(ReloadPhase::Await { slot }) = self.pctl.state {
            let ready = slot.is_none_or(|s| self.slot_is_serving(s));
            if ready {
                self.reload_quit_next(now);
            }
        }
    }

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
        tracing::info!(
            target: "master",
            "status: {} running, {} idle, generation {}",
            self.table.running(),
            self.idle_count(),
            self.table.generation
        );
        for s in snap {
            tracing::info!(
                target: "master",
                "  slot {} pid {} state {} handled {} errors {} recycles {}",
                s.id, s.pid, s.state, s.handled, s.errors, s.recycles
            );
        }
    }

    /// Failboot only for a gen-0 worker in a pool that never served: a reload replacement dying unhealthy must not take down the running pool.
    fn on_child_exit(
        &mut self,
        w: WorkerProc,
        verdict: ExitVerdict,
        now: Instant,
    ) -> anyhow::Result<()> {
        let slot = w.slot;
        let lived = now.saturating_duration_since(w.spawned_at);
        self.latch_served();
        self.scoreboard.clear(slot);

        if let PctlState::Reloading(ReloadPhase::Drain { draining, .. }) = self.pctl.state
            && draining == w.pid
        {
            if self.has_old_gen() {
                self.reload_enter_await(Some(slot), now);
            } else {
                self.reload_finish();
            }
            return Ok(());
        }
        if self.pctl.is_stopping() {
            return Ok(());
        }

        match verdict {
            ExitVerdict::IdleKill => {}
            ExitVerdict::Recycle | ExitVerdict::Drain | ExitVerdict::TimeoutKill => {
                self.table.slots[slot].schedule_immediate(now);
                self.apply_respawn_gate(slot);
            }
            ExitVerdict::Unhealthy => {
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

    /// Ondemand re-forks from demand; crash and unhealthy backoff deadlines are kept as fork suppression until they expire, throttling a fork-crash loop.
    fn apply_respawn_gate(&mut self, slot: usize) {
        if matches!(self.cfg.scaling, Scaling::Ondemand) {
            self.table.slots[slot].cancel_respawn();
        }
    }

    fn idle_kill_pid(&mut self, pid: libc::pid_t) {
        if let Some(p) = self.table.procs.iter_mut().find(|p| p.pid == pid) {
            if p.kill_intent == Some(KillIntent::Idle) {
                kill(pid, libc::SIGKILL);
            } else {
                kill(pid, libc::SIGQUIT);
                p.kill_intent = Some(KillIntent::Idle);
            }
        }
    }

    /// TERM an ACTIVE worker past `request_terminate_timeout`, KILL if it is still ACTIVE a tick later (TERM could not land, uninterruptible sleep); Acquire on the state load orders the timestamp read after it.
    fn watchdog_tick(&mut self) {
        let limit = self.cfg.request_terminate_timeout;
        if limit.is_zero() {
            return;
        }
        let now_ms = now_millis();
        for p in self.table.procs.iter_mut() {
            let s = self.scoreboard.slot(p.slot);
            if s.state.load(Acquire) != SLOT_ACTIVE {
                continue;
            }
            let age_ms = u128::from(now_ms.saturating_sub(s.last_activity_ms.load(Relaxed)));
            if age_ms < limit.as_millis() {
                continue;
            }
            if p.kill_intent == Some(KillIntent::Timeout) {
                kill(p.pid, libc::SIGKILL);
            } else {
                tracing::warn!(
                    target: "master",
                    "worker {} exceeded request_terminate_timeout_secs ({}s); terminating",
                    p.pid,
                    limit.as_secs()
                );
                kill(p.pid, libc::SIGTERM);
                p.kill_intent = Some(KillIntent::Timeout);
            }
        }
    }

    fn maintenance_tick(&mut self, now: Instant) {
        self.latch_served();
        self.watchdog_tick();
        match self.cfg.scaling {
            Scaling::Static => self.static_refill(now),
            Scaling::Dynamic {
                min_spare,
                max_spare,
            } => self.dynamic_maintenance(min_spare, max_spare, now),
            Scaling::Ondemand => self.ondemand_maintenance(),
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
                    tracing::warn!(
                        target: "master",
                        "reached pool.processes ceiling ({}), consider raising it",
                        self.cfg.processes
                    );
                }
            }
            DynAction::Steady => {}
        }
    }

    /// Trims the idle worker with the stalest activity: by process age, a busy older worker could shield a long-expired younger one indefinitely.
    fn ondemand_maintenance(&mut self) {
        let target = self
            .table
            .procs
            .iter()
            .filter(|p| self.slot_is_idle(p.slot))
            .map(|p| {
                let s = self.scoreboard.slot(p.slot);
                (p.pid, s.last_activity_ms.load(Relaxed))
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

    /// In the reload Await gate: re-probe the replacement and force past a stuck one at the safety cap; otherwise drive QUIT/TERM/KILL escalation.
    fn on_pctl_deadline(&mut self, now: Instant) {
        let PctlState::Reloading(ReloadPhase::Await { slot }) = self.pctl.state else {
            self.escalate(now);
            return;
        };
        self.reload_try_advance(now);
        if let PctlState::Reloading(ReloadPhase::Await { .. }) = self.pctl.state {
            if self.reload_await_until.is_some_and(|t| now >= t) {
                if let Some(s) = slot {
                    tracing::warn!(
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
                if !matches!(self.cfg.scaling, Scaling::Ondemand) {
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

    /// Arm only when a fork could land: a readable level-triggered listener would otherwise busy-spin poll through the backoff window.
    fn compute_armed(&self) -> bool {
        if !matches!(self.cfg.scaling, Scaling::Ondemand) {
            return false;
        }
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
                    continue;
                }
                anyhow::bail!("poll: {}", std::io::Error::last_os_error());
            }
            let now = Instant::now();

            let mut got_chld: bool = false;
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

            if got_chld {
                for (w, verdict) in self.table.reap_all() {
                    self.on_child_exit(w, verdict, now)?;
                }
                if self.pctl.is_stopping() && self.table.procs.is_empty() {
                    return Ok(StopReason::Drained);
                }
            }

            if n > 0 && fds.len() > 1 && self.compute_armed() {
                let readable = fds[1..].iter().any(|p| (p.revents & libc::POLLIN) != 0);
                if readable {
                    self.ondemand_fork_one(now);
                }
            }

            self.fire_due_deadlines(now);

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
        assert_eq!(
            at(base + Duration::from_micros(1500)).poll_timeout_ms(base),
            2
        );
        assert_eq!(at(base + Duration::from_nanos(1)).poll_timeout_ms(base), 1);
        assert_eq!(at(base + Duration::from_millis(3)).poll_timeout_ms(base), 3);
        assert_eq!(at(base).poll_timeout_ms(base + Duration::from_millis(5)), 0);
    }

    use crate::process::WorkerProc;
    use std::os::fd::{FromRawFd, OwnedFd};

    // Sentinel pids above PID_MAX_LIMIT: the QUITs these tests send resolve to ESRCH.
    const P_OLD0: libc::pid_t = 2_000_000_001;
    const P_OLD1: libc::pid_t = 2_000_000_002;
    const P_NEW: libc::pid_t = 2_000_000_100;

    fn dummy_self_pipe() -> SelfPipe {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: socketpair fills a 2-element array with two owned fds.
        let rc = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "socketpair");
        SelfPipe {
            // SAFETY: fds holds two fresh fds we take sole ownership of.
            rd: unsafe { OwnedFd::from_raw_fd(fds[0]) },
            wr: unsafe { OwnedFd::from_raw_fd(fds[1]) },
        }
    }

    fn test_master(nslots: usize, scaling: Scaling) -> Master<impl FnMut(WorkerEnv) -> i32> {
        let scoreboard: Scoreboard = Scoreboard::create(nslots).unwrap();
        let cfg: MasterConfig = MasterConfig {
            processes: 3,
            scaling,
            process_idle_timeout: Duration::from_secs(10),
            process_control_timeout: Duration::from_secs(30),
            request_terminate_timeout: Duration::ZERO,
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
            kill_intent: None,
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
        let mut m = test_master(6, Scaling::Static);
        let t0 = Instant::now();
        m.table.generation = 1;
        for (i, &pid) in [P_OLD0, P_OLD1, 2_000_000_003].iter().enumerate() {
            push_proc(&mut m, pid, i, 0, t0 + Duration::from_millis(i as u64));
            set_slot(&m, i, SLOT_IDLE);
        }
        push_proc(&mut m, P_NEW, 3, 1, t0 + Duration::from_millis(10));
        set_slot(&m, 3, SLOT_STARTING);
        m.pctl.set_reload_await(Some(3));
        m.reload_await_until = Some(t0 + Duration::from_secs(30));

        m.reload_try_advance(t0);
        assert!(matches!(
            m.pctl.state,
            PctlState::Reloading(ReloadPhase::Await { slot: Some(3) })
        ));

        set_slot(&m, 3, SLOT_IDLE);
        m.reload_try_advance(t0);
        assert!(matches!(
            m.pctl.state,
            PctlState::Reloading(ReloadPhase::Drain { draining, .. }) if draining == P_OLD0
        ));
    }

    /// Under load a replacement can be ACTIVE at every probe; the gate must accept that as serving.
    #[test]
    fn reload_gate_opens_on_active_replacement() {
        let mut m = test_master(6, Scaling::Static);
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
        let mut m = test_master(6, Scaling::Static);
        let t0 = Instant::now();
        m.table.generation = 1;
        push_proc(&mut m, P_OLD0, 0, 0, t0);
        set_slot(&m, 0, SLOT_IDLE);
        push_proc(&mut m, P_NEW, 3, 1, t0);
        set_slot(&m, 3, SLOT_STARTING);
        m.pctl.set_reload_await(Some(3));
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
        let mut m = test_master(6, Scaling::Static);
        let t0 = Instant::now();
        m.table.generation = 1;
        push_proc(&mut m, P_OLD0, 0, 0, t0);
        set_slot(&m, 0, SLOT_IDLE);
        push_proc(&mut m, P_NEW, 3, 1, t0);
        set_slot(&m, 3, SLOT_STARTING);
        m.pctl.set_reload_await(Some(3));
        m.reload_await_until = Some(t0 + Duration::from_secs(30));

        let now = t0 + Duration::from_millis(1);
        m.on_pctl_deadline(now);
        assert!(matches!(
            m.pctl.state,
            PctlState::Reloading(ReloadPhase::Await { slot: Some(3) })
        ));
        assert_eq!(m.deadlines.pctl, Some(now + RELOAD_GATE_POLL));
    }

    #[test]
    fn reload_finishes_when_last_old_worker_reaped() {
        let mut m = test_master(6, Scaling::Static);
        let t0 = Instant::now();
        m.table.generation = 1;
        push_proc(&mut m, P_NEW, 0, 1, t0);
        let drained = WorkerProc {
            pid: P_OLD0,
            slot: 1,
            generation: 0,
            spawned_at: t0,
            kill_intent: None,
        };
        m.pctl.set_reload_drain(P_OLD0);
        m.deadlines.pctl = Some(t0 + Duration::from_secs(30));

        m.on_child_exit(drained, ExitVerdict::Drain, t0).unwrap();
        assert!(m.pctl.is_normal());
        assert_eq!(m.deadlines.pctl, None);
        assert_eq!(m.reload_await_until, None);
        assert_eq!(m.table.procs.len(), 1);
    }

    #[test]
    fn ondemand_reload_paces_one_at_a_time_without_spawning() {
        let mut m = test_master(6, Scaling::Ondemand);
        let t0 = Instant::now();
        for (i, &pid) in [P_OLD0, P_OLD1].iter().enumerate() {
            push_proc(&mut m, pid, i, 0, t0 + Duration::from_millis(i as u64));
            set_slot(&m, i, SLOT_IDLE);
        }

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

        let i = m.table.procs.iter().position(|p| p.pid == P_OLD0).unwrap();
        let w0 = m.table.procs.swap_remove(i);
        m.on_child_exit(w0, ExitVerdict::Drain, t0).unwrap();
        assert_eq!(m.table.procs.len(), 1);
        assert!(matches!(
            m.pctl.state,
            PctlState::Reloading(ReloadPhase::Drain { draining, .. }) if draining == P_OLD1
        ));

        let i = m.table.procs.iter().position(|p| p.pid == P_OLD1).unwrap();
        let w1 = m.table.procs.swap_remove(i);
        m.on_child_exit(w1, ExitVerdict::Drain, t0).unwrap();
        assert!(m.pctl.is_normal());
        assert_eq!(m.table.procs.len(), 0);
    }

    fn dead_worker(pid: libc::pid_t, slot: usize, generation: u32, at: Instant) -> WorkerProc {
        WorkerProc {
            pid,
            slot,
            generation,
            spawned_at: at,
            kill_intent: None,
        }
    }

    #[test]
    fn unhealthy_after_ever_served_respawns_not_failboot() {
        let mut m = test_master(3, Scaling::Static);
        let t0 = Instant::now();
        m.ever_served = true;
        let w = dead_worker(4242, 0, 0, t0);
        let r = m.on_child_exit(w, ExitVerdict::Unhealthy, t0 + Duration::from_secs(1));
        assert!(r.is_ok());
        assert!(m.table.slots[0].respawn_at.is_some());
    }

    #[test]
    fn gen0_unhealthy_never_served_failboots() {
        let mut m = test_master(3, Scaling::Static);
        let t0 = Instant::now();
        assert!(!m.ever_served);
        let w = dead_worker(4243, 0, 0, t0);
        let r = m.on_child_exit(w, ExitVerdict::Unhealthy, t0);
        assert!(r.is_err());
    }

    #[test]
    fn gen1_unhealthy_never_served_respawns_not_failboot() {
        let mut m = test_master(3, Scaling::Static);
        let t0 = Instant::now();
        m.table.generation = 1;
        assert!(!m.ever_served);
        let w = dead_worker(4244, 0, 1, t0);
        let r = m.on_child_exit(w, ExitVerdict::Unhealthy, t0);
        assert!(r.is_ok());
        assert!(m.table.slots[0].respawn_at.is_some());
    }

    #[test]
    fn watchdog_marks_an_overdue_active_worker_for_timeout() {
        let mut m = test_master(3, Scaling::Static);
        m.cfg.request_terminate_timeout = Duration::from_secs(2);
        let t0 = Instant::now();
        push_proc(&mut m, P_OLD0, 0, 0, t0);
        set_slot(&m, 0, SLOT_ACTIVE);
        m.scoreboard.slots()[0]
            .last_activity_ms
            .store(now_millis().saturating_sub(5_000), Relaxed);

        m.watchdog_tick();
        assert_eq!(
            m.table.procs[0].kill_intent,
            Some(KillIntent::Timeout),
            "the overdue active worker must have timeout intent"
        );
    }

    #[test]
    fn watchdog_spares_fresh_active_and_idle_workers() {
        let mut m = test_master(3, Scaling::Static);
        m.cfg.request_terminate_timeout = Duration::from_secs(2);
        let t0 = Instant::now();
        push_proc(&mut m, P_OLD0, 0, 0, t0);
        set_slot(&m, 0, SLOT_ACTIVE);
        m.scoreboard.slots()[0]
            .last_activity_ms
            .store(now_millis(), Relaxed);
        push_proc(&mut m, P_OLD1, 1, 0, t0);
        set_slot(&m, 1, SLOT_IDLE);
        m.scoreboard.slots()[1]
            .last_activity_ms
            .store(now_millis().saturating_sub(60_000), Relaxed);

        m.watchdog_tick();
        assert!(m.table.procs.iter().all(|p| p.kill_intent.is_none()));
    }

    /// A master-chosen kill respawns immediately: no failboot (that is for Unhealthy only) and no backoff.
    #[test]
    fn timeout_kill_respawns_immediately_without_failboot() {
        let mut m = test_master(3, Scaling::Static);
        let t0 = Instant::now();
        assert!(!m.ever_served);
        let w = dead_worker(4246, 0, 0, t0);
        let r = m.on_child_exit(w, ExitVerdict::TimeoutKill, t0);
        assert!(r.is_ok());
        assert_eq!(m.table.slots[0].respawn_at, Some(t0));
        assert_eq!(m.table.slots[0].crash_streak, 0);
    }

    #[test]
    fn ondemand_crash_backoff_suppresses_without_respawn() {
        let mut m = test_master(2, Scaling::Ondemand);
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

    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    /// Counts WARN events on the `master` target; hand-rolled because the crate depends only on the `tracing` facade.
    #[derive(Clone, Default)]
    struct WarnCounter(Arc<AtomicUsize>);

    impl tracing::Subscriber for WarnCounter {
        fn enabled(&self, md: &tracing::Metadata) -> bool {
            md.target() == "master" && *md.level() == tracing::Level::WARN
        }
        fn event(&self, _: &tracing::Event) {
            self.0.fetch_add(1, Relaxed);
        }
        fn new_span(&self, _: &tracing::span::Attributes) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[test]
    fn dynamic_ceiling_warns_once_and_never_spawns() {
        let mut m = test_master(
            4,
            Scaling::Dynamic {
                min_spare: 2,
                max_spare: 4,
            },
        );
        m.cfg.processes = 2;
        let t0 = Instant::now();
        for (i, &pid) in [P_OLD0, P_OLD1].iter().enumerate() {
            push_proc(&mut m, pid, i, 0, t0);
            set_slot(&m, i, SLOT_ACTIVE);
        }

        let warns = WarnCounter::default();
        tracing::subscriber::with_default(warns.clone(), || {
            m.maintenance_tick(t0);
            m.maintenance_tick(t0 + Duration::from_secs(1));
        });
        assert_eq!(warns.0.load(Relaxed), 1, "ceiling warning must fire once");
        assert!(m.warned_max_children);
        assert_eq!(m.table.procs.len(), 2, "the ceiling ticks must not spawn");
    }

    #[test]
    fn static_refill_counts_pending_respawn_as_committed() {
        let mut m = test_master(2, Scaling::Static);
        let t0 = Instant::now();
        m.cfg.processes = 1;
        m.table.slots[0].schedule_immediate(t0);

        m.maintenance_tick(t0);
        assert!(
            m.table.procs.is_empty(),
            "the pending respawn already covers the target"
        );
        assert_eq!(m.table.slots[0].respawn_at, Some(t0), "deadline untouched");
    }

    use crate::signals::SIG_TERM;

    #[test]
    fn ondemand_arms_only_when_a_fork_can_land() {
        let mut m = test_master(2, Scaling::Ondemand);
        assert!(m.compute_armed());

        let t0 = Instant::now();
        for s in &mut m.table.slots {
            s.schedule_backoff(Duration::ZERO, t0);
        }
        assert!(!m.compute_armed());
    }

    #[test]
    fn ondemand_disarms_while_stopping() {
        let mut m = test_master(2, Scaling::Ondemand);
        assert!(m.compute_armed());
        m.pctl.on_signal(SIG_TERM);
        assert!(!m.compute_armed());
    }

    /// Static and dynamic workers accept in the children; the master watches only its self-pipe.
    #[test]
    fn non_ondemand_never_arms_listeners() {
        let m = test_master(2, Scaling::Static);
        assert!(!m.compute_armed());
    }
}
