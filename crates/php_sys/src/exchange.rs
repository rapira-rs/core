//! The Rust half of `Rapira\Internal\Http\Exchange`: owns the `Job` while PHP
//! holds the unit, marshals the request out and the response verbs in, and
//! seals through the existing single-Frame `Context::finish` path.

use std::{
    cell::Cell,
    ffi::{c_char, c_int, c_void},
    io::Read,
    time::Duration,
};

use crate::{
    RAPIRA_RECV_BUSY, RAPIRA_RECV_CLOSED, RAPIRA_RECV_EMPTY, RAPIRA_RECV_OK, RAPIRA_RECV_TIMEOUT,
    RAPIRA_VERB_FINALIZED, RAPIRA_VERB_HEAD_WRITTEN, RAPIRA_VERB_INTERIM, RAPIRA_VERB_INVALID,
    RAPIRA_VERB_OK, RAPIRA_VERB_OVERFLOW,
    callbacks::{MAX_BUFFERED_BODY, guard},
    rapira_receive_timed, rapira_receive_untimed,
    scoreboard::{Event, sb_update},
    start::{Pulled, pending_depth, pull_job_try, pull_job_wait},
    types::{Job, ResponseHead, StreamState},
};

/// The unit machine. Both live variants carry the Box pointer: free_obj
/// normally reclaims it, and tracking it here covers the paths where free_obj
/// cannot run (a bailed php_request_shutdown, a bailout between Box::into_raw
/// and the C-side store) — a leaked unfinalized unit would hang its client
/// forever.
#[derive(Clone, Copy)]
enum Unit {
    /// No live unit.
    Idle,
    /// An unfinalized unit is out with PHP; receive verbs return BUSY.
    Handling(*mut ExchangeState),
    /// Sealed, but the PHP object still owns the Box; free_obj reclaims it.
    Sealed(*mut ExchangeState),
}

/// Dispatcher state for the current cycle: the unit machine plus two sticky
/// latches feeding run_cycle's exit decision.
#[derive(Clone, Copy)]
struct CycleState {
    unit: Unit,
    /// receive() observed channel closure this cycle.
    closed_seen: bool,
    /// A unit was finalized this cycle.
    served: bool,
}

const CYCLE_IDLE: CycleState = CycleState {
    unit: Unit::Idle,
    closed_seen: false,
    served: false,
};

thread_local! {
    static CYCLE: Cell<CycleState> = const { Cell::new(CYCLE_IDLE) };
}

fn update(f: impl FnOnce(&mut CycleState)) {
    let mut c = CYCLE.get();
    f(&mut c);
    CYCLE.set(c);
}

pub(crate) fn cycle_reset() {
    reclaim_current();
    CYCLE.set(CYCLE_IDLE);
}

/// Reclaim a unit free_obj never saw (shutdown bailout / allocation bailout).
pub(crate) fn reclaim_current() {
    if let Unit::Handling(ptr) | Unit::Sealed(ptr) = CYCLE.get().unit {
        update(|c| c.unit = Unit::Idle);
        // SAFETY: the machine only holds pointers from Box::into_raw in
        // finish_pull, and exchange_drop untracks before reclaiming there.
        let st = unsafe { Box::from_raw(ptr) };
        // an unfinalized reclaim is a failed unit: count it like exchange_drop
        if st.stage != Stage::Finalized {
            sb_update(Event::Handled(true));
        }
        drop(st);
    }
}

pub(crate) fn closed_seen() -> bool {
    CYCLE.get().closed_seen
}

pub(crate) fn served_any() -> bool {
    CYCLE.get().served
}

/// Response progress. The head locks on the first head OR body write (per the
/// contract, a body chunk commits an implicit 200 first), and Finalized implies
/// a head exists — seal() is only reachable with the head committed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Open,
    HeadCommitted,
    Finalized,
}

pub struct ExchangeState {
    job: Box<Job>,
    /// Request body, read out of the Job's reader at construction.
    body: Vec<u8>,
    /// Absolute-form URI synthesized for `Request::$uri`.
    uri_abs: String,
    /// `Request::$authority` (Host header), if any.
    authority: Option<String>,
    stage: Stage,
}

impl ExchangeState {
    fn new(mut job: Box<Job>) -> Self {
        let mut body = Vec::new();
        // The Job's body is an in-memory Cursor today; a read error still must
        // not kill the worker — serve an empty body and log.
        if let Err(e) = job.ctx.req.body.read_to_end(&mut body) {
            tracing::error!(target: "rapira", "request body read failed: {e}");
            body.clear();
        }
        let authority = job
            .ctx
            .req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("host"))
            .map(|(_, v)| String::from_utf8_lossy(v).into_owned());
        let scheme = if job.ctx.req.https { "https" } else { "http" };
        let fallback = format!("{}:{}", job.ctx.req.server_name, job.ctx.req.server_port);
        // Asterisk-form (`OPTIONS *`) and CONNECT authority-form targets are not
        // paths; the contract has $uri fall back to the authority root there.
        let path = if job.ctx.req.uri.starts_with('/') {
            job.ctx.req.uri.as_str()
        } else {
            "/"
        };
        let uri_abs = format!(
            "{scheme}://{}{path}",
            authority.as_deref().unwrap_or(&fallback)
        );
        Self {
            job,
            body,
            uri_abs,
            authority,
            stage: Stage::Open,
        }
    }
}

// Keep in sync with rapira_str in wrapper.h.
#[repr(C)]
pub struct RapiraStr {
    pub ptr: *const c_char,
    pub len: usize,
}

fn s(v: &str) -> RapiraStr {
    RapiraStr {
        ptr: v.as_ptr().cast(),
        len: v.len(),
    }
}

fn b(v: &[u8]) -> RapiraStr {
    RapiraStr {
        ptr: v.as_ptr().cast(),
        len: v.len(),
    }
}

// Keep in sync with rapira_request_view in wrapper.h.
#[repr(C)]
pub struct RapiraRequestView {
    pub method: RapiraStr,
    pub uri: RapiraStr,
    pub target: RapiraStr,
    /// ptr NULL when the request named no authority.
    pub authority: RapiraStr,
    pub protocol: RapiraStr,
    pub body: RapiraStr,
    pub remote_ip: RapiraStr,
    pub server_ip: RapiraStr,
    /// 0 = not an IP endpoint (C builds UnixAddress(null)).
    pub remote_port: i32,
    pub server_port: i32,
    pub received_at: f64,
    pub header_count: usize,
}

/// # Safety
/// `job` from receive; `out` writable. Views borrow the ExchangeState — C must
/// copy into zend_strings before the next FFI call that could free the job.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_request(
    job: *const c_void,
    out: *mut RapiraRequestView,
) {
    guard((), || {
        let st = unsafe { &*job.cast::<ExchangeState>() };
        let req = &st.job.ctx.req;
        let authority = st.authority.as_deref().map_or(
            RapiraStr {
                ptr: std::ptr::null(),
                len: 0,
            },
            s,
        );
        unsafe {
            out.write(RapiraRequestView {
                method: s(&req.method),
                uri: s(&st.uri_abs),
                target: s(&req.uri),
                authority,
                protocol: s(&req.protocol),
                body: b(&st.body),
                remote_ip: s(&req.remote_addr),
                server_ip: s(&req.server_name),
                remote_port: req
                    .remote_port
                    .parse::<i32>()
                    .ok()
                    .filter(|p| (1..=65535).contains(p))
                    .unwrap_or(0),
                server_port: req
                    .server_port
                    .parse::<i32>()
                    .ok()
                    .filter(|p| (1..=65535).contains(p))
                    .unwrap_or(0),
                received_at: req.received_at,
                header_count: req.headers.len(),
            });
        }
    })
}

/// # Safety
/// `i < header_count`; out params writable; same borrow rule as the view.
/// Out-of-range or a caught panic leaves the outs untouched — C zero-inits
/// and skips NULL names.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_header(
    job: *const c_void,
    i: usize,
    name: *mut RapiraStr,
    value: *mut RapiraStr,
) {
    guard((), || {
        let st = unsafe { &*job.cast::<ExchangeState>() };
        let Some((n, v)) = st.job.ctx.req.headers.get(i) else {
            return;
        };
        unsafe {
            name.write(s(n));
            value.write(b(v));
        }
    })
}

/// # Safety
/// `out_job` writable. On RAPIRA_RECV_OK it receives a Box<ExchangeState>.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_receive(timeout_us: i64, out_job: *mut *mut c_void) -> c_int {
    guard(RAPIRA_RECV_CLOSED as c_int, || {
        // busy check before the disarm: the handling unit keeps its budget
        if matches!(CYCLE.get().unit, Unit::Handling(_)) {
            return RAPIRA_RECV_BUSY as c_int;
        }
        // SAFETY: plain zend timer bookkeeping on this thread; no bailout path.
        unsafe { rapira_receive_untimed() };
        let pulled = match timeout_us {
            -1 => pull_job_wait(None),
            0 => pull_job_try(),
            t => pull_job_wait(Some(Duration::from_micros(t as u64))),
        };
        // Timeout/Empty/Closed stay untimed until the next receive verb
        // (ledgered): the worker is between units, not executing one.
        finish_pull(pulled, /*empty_is_timeout=*/ true, out_job)
    })
}

/// # Safety
/// As `rapira_rs_receive`; never blocks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_try_receive(out_job: *mut *mut c_void) -> c_int {
    guard(RAPIRA_RECV_CLOSED as c_int, || {
        if matches!(CYCLE.get().unit, Unit::Handling(_)) {
            return RAPIRA_RECV_BUSY as c_int;
        }
        // Same discipline as receive(): a polling loop is waiting for work,
        // and waiting never counts against the per-unit budget.
        // SAFETY: as above.
        unsafe { rapira_receive_untimed() };
        finish_pull(pull_job_try(), /*empty_is_timeout=*/ false, out_job)
    })
}

fn finish_pull(pulled: Pulled, empty_is_timeout: bool, out_job: *mut *mut c_void) -> c_int {
    match pulled {
        Pulled::Job(job) => {
            let ptr = Box::into_raw(Box::new(ExchangeState::new(job)));
            // a previous Sealed unit becomes free_obj's sole responsibility:
            // its frame is already delivered, so worst case is a leak, not a hang
            update(|c| c.unit = Unit::Handling(ptr));
            unsafe { out_job.write(ptr.cast()) };
            // Arm the captured budget only once the unit is owned and handed
            // out; from here PHP is executing on the clock.
            // SAFETY: plain zend timer bookkeeping; no bailout path.
            unsafe { rapira_receive_timed() };
            RAPIRA_RECV_OK as c_int
        }
        Pulled::Timeout => RAPIRA_RECV_TIMEOUT as c_int,
        Pulled::Empty if empty_is_timeout => RAPIRA_RECV_TIMEOUT as c_int,
        Pulled::Empty => RAPIRA_RECV_EMPTY as c_int,
        Pulled::Closed => {
            update(|c| c.closed_seen = true);
            RAPIRA_RECV_CLOSED as c_int
        }
    }
}

/// # Safety
/// `pending`/`active` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_dispatcher_counters(pending: *mut i64, active: *mut i64) {
    guard((), || unsafe {
        pending.write(pending_depth() as i64);
        active.write(i64::from(matches!(CYCLE.get().unit, Unit::Handling(_))));
    })
}

/// # Safety
/// `job` from receive; `pairs` = 2*npairs valid RapiraStr (name,value
/// alternating). Status already range-checked by C (100..=599).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_write_head(
    job: *mut c_void,
    status: u16,
    pairs: *const RapiraStr,
    npairs: usize,
) -> c_int {
    guard(RAPIRA_VERB_INVALID as c_int, || {
        let st = unsafe { &mut *job.cast::<ExchangeState>() };
        // Finalized implies a committed head, so one gate covers both.
        if st.stage != Stage::Open {
            return RAPIRA_VERB_HEAD_WRITTEN as c_int;
        }
        // 101 ends the HTTP conversation and counts as a final head; the other
        // 1xx are interim — advisory, and this host emits none yet.
        if status != 101 && (100..200).contains(&status) {
            return RAPIRA_VERB_INTERIM as c_int;
        }
        let mut headers = Vec::with_capacity(npairs);
        for i in 0..npairs {
            let name = unsafe { &*pairs.add(2 * i) };
            let value = unsafe { &*pairs.add(2 * i + 1) };
            let name = unsafe { std::slice::from_raw_parts(name.ptr.cast::<u8>(), name.len) };
            let value = unsafe { std::slice::from_raw_parts(value.ptr.cast::<u8>(), value.len) };
            headers.push((String::from_utf8_lossy(name).into_owned(), value.to_vec()));
        }
        // Direct set, not Context::commit_head — that path implements the CGI
        // `Status:` override, which must not exist on the Exchange surface.
        st.job.ctx.head = Some(ResponseHead { status, headers });
        st.job.ctx.stream = StreamState::HeadSent;
        st.stage = Stage::HeadCommitted;
        RAPIRA_VERB_OK as c_int
    })
}

/// # Safety
/// `job` from receive; `p` points at `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_write_body(
    job: *mut c_void,
    p: *const c_char,
    len: usize,
    eos: bool,
) -> c_int {
    guard(RAPIRA_VERB_INVALID as c_int, || {
        let st = unsafe { &mut *job.cast::<ExchangeState>() };
        if st.stage == Stage::Finalized {
            return RAPIRA_VERB_FINALIZED as c_int;
        }
        // The contract commits an implicit 200 before the first body chunk,
        // even a buffered eos:false one — a later writeHead must see
        // HeadAlreadyWritten, not retroactively restamp the status.
        if st.stage == Stage::Open {
            st.job.ctx.head = Some(ResponseHead {
                status: 200,
                headers: Vec::new(),
            });
            st.job.ctx.stream = StreamState::HeadSent;
            st.stage = Stage::HeadCommitted;
        }
        let ctx = &mut st.job.ctx;
        if len > 0 {
            if ctx.body.len() + len > MAX_BUFFERED_BODY {
                // Seal as truncated so the unit concludes (upstream reports the
                // failure) instead of wedging the worker on the next receive().
                seal(st, /*truncated=*/ true);
                return RAPIRA_VERB_OVERFLOW as c_int;
            }
            let bytes = unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) };
            ctx.body.extend_from_slice(bytes);
        }
        if eos {
            seal(st, /*truncated=*/ false);
        }
        RAPIRA_VERB_OK as c_int
    })
}

fn seal(st: &mut ExchangeState, truncated: bool) {
    let ctx = &mut st.job.ctx;
    // Only write_body reaches here, and it commits the head first.
    let status = ctx.head.as_ref().map_or(200, |h| h.status);
    // RFC 9112 §6.3: no body on 204/304 or any HEAD response — chunks were
    // accepted, now dropped, so HEAD shares the GET code path.
    // https://www.rfc-editor.org/rfc/rfc9112#section-6.3
    if matches!(status, 204 | 304) || ctx.req.method.eq_ignore_ascii_case("HEAD") {
        ctx.body.clear();
    }
    st.stage = Stage::Finalized;
    update(|c| {
        if let Unit::Handling(p) = c.unit {
            c.unit = Unit::Sealed(p);
        }
        c.served = true;
    });
    sb_update(Event::Handled(truncated));
    ctx.finish(truncated);
}

/// # Safety
/// `job` from receive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_is_finalized(job: *const c_void) -> bool {
    guard(true, || unsafe {
        (*job.cast::<ExchangeState>()).stage == Stage::Finalized
    })
}

/// Reclaims the Box when PHP frees the Exchange object (free_obj). An
/// unfinalized drop fails the unit: the Frame sender dies unsent and the
/// runtime reports "worker died mid-response" upstream.
/// # Safety
/// `job` is NULL or a pointer produced by `Box::into_raw` in receive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_drop(job: *mut c_void) {
    guard((), || {
        if job.is_null() {
            return;
        }
        let ptr: *mut ExchangeState = job.cast();
        // untrack before the reclaim so a later reclaim_current cannot double-free
        update(|c| {
            if matches!(c.unit, Unit::Handling(p) | Unit::Sealed(p) if p == ptr) {
                c.unit = Unit::Idle;
            }
        });
        let st = unsafe { Box::from_raw(ptr) };
        if st.stage != Stage::Finalized {
            sb_update(Event::Handled(true));
        }
        drop(st);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Context, Request};
    use std::path::PathBuf;

    fn state() -> (
        ExchangeState,
        tokio::sync::mpsc::Receiver<crate::types::Frame>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let req = Request {
            method: String::new(),
            uri: "/".into(),
            https: false,
            query: String::new(),
            protocol: String::new(),
            remote_addr: String::new(),
            server_name: String::new(),
            server_port: String::new(),
            remote_port: String::new(),
            script_name: String::new(),
            document_root: String::new(),
            script_filename: PathBuf::new(),
            headers: Vec::new(),
            server_vars: Vec::new(),
            content_type: None,
            content_length: 0,
            body: Box::new(std::io::empty()),
            received_at: 0.0,
        };
        let job = Box::new(Job {
            ctx: Context::new(req, tx),
        });
        (ExchangeState::new(job), rx)
    }

    /// The buffer cap must seal (truncated) rather than merely error: an
    /// unsealed overflow leaves the unit in Handling and wedges every later
    /// receive() on the single-flight check for the life of the worker. The
    /// oversized `len` is checked before the byte slice is formed, so no giant
    /// buffer is needed.
    #[test]
    fn overflow_seals_the_unit_truncated() {
        let (mut st, mut rx) = state();
        let job: *mut c_void = (&raw mut st).cast();
        let rc = unsafe {
            rapira_rs_exchange_write_body(job, c"x".as_ptr(), MAX_BUFFERED_BODY + 1, false)
        };
        assert_eq!(rc, RAPIRA_VERB_OVERFLOW as c_int);

        let frame = rx.blocking_recv().expect("overflow must seal a frame");
        assert!(frame.truncated, "the sealed frame reports truncation");
        assert_eq!(frame.head.map(|h| h.status), Some(200));

        // The unit is concluded: later verbs see FINALIZED, not a wedge.
        let rc = unsafe { rapira_rs_exchange_write_body(job, c"y".as_ptr(), 1, true) };
        assert_eq!(rc, RAPIRA_VERB_FINALIZED as c_int);
        assert!(unsafe { rapira_rs_exchange_is_finalized(job) });
    }
}
