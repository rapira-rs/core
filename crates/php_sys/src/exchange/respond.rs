//! The response verbs: frame transport, head/body/trailer cores, seal,
//! lifecycle probes, and the free_obj reclaim.

use super::headers::{forbidden_trailer, split_framing, strip_framing, walk_head_table};
use super::*;

/// Verb outcomes; only the non-Ok arms surface to PHP, as throws. The cores
/// return these instead of throwing so no owned state is live when
/// `zend_throw_*` (which can bailout) runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Verb {
    Ok,
    /// Advisory 1xx head, emitted; nothing thrown.
    Interim,
    Finalized,
    HeadWritten,
    Overflow,
    /// The host closed the exchange first (gate 2, or a failed send).
    Discarded,
    ContentLengthExceeded,
    /// An argument `\ValueError` discovered past the shell's table walk.
    BadField(&'static CStr),
    /// `sendFile` gate 4b: raised before anything is written, catchable.
    FileNotSendable(&'static CStr),
    /// Trailers may only end a response with a committed final head.
    HeadNotWritten,
}

/// # Safety
/// Engine active; can bailout on OOM.
pub(super) unsafe fn throw_verb(v: Verb) {
    unsafe {
        match v {
            Verb::Ok | Verb::Interim => {}
            Verb::Finalized => zend::throw_exception(
                rapira_ce_already_finalized_error,
                c"the response already ended",
            ),
            Verb::HeadWritten => zend::throw_exception(
                rapira_ce_http_head_already_written_error,
                c"the final head has already been written",
            ),
            // the unit was sealed as truncated; the worker is not wedged
            Verb::Overflow => zend::throw_error(c"response chunk exceeds the host buffer cap"),
            Verb::Discarded => zend::throw_exception(
                rapira_ce_work_discarded_exception,
                c"the host closed the exchange first",
            ),
            Verb::ContentLengthExceeded => zend::throw_exception(
                rapira_ce_http_content_length_exceeded_error,
                c"the write goes past the content-length the head declared",
            ),
            Verb::BadField(msg) => zend::throw_value_error(msg),
            Verb::HeadNotWritten => zend::throw_exception(
                rapira_ce_http_head_not_written_error,
                c"no final head has been committed yet",
            ),
            Verb::FileNotSendable(msg) => {
                zend::throw_exception(rapira_ce_http_file_not_sendable_exception, msg);
            }
        }
    }
}

pub(super) struct Closed;

/// Push a frame; on a full channel, park with the wall timer disarmed. A
/// parked thread never reaches an opcode boundary, so a fired timeout could
/// not longjmp anyway - and on NTS its second expiry would `_exit(124)` the
/// process. The re-arm grants the remaining budget (floor 1s), so
/// max_execution_time keeps bounding compute while park time is excluded.
/// # Safety
/// Engine active on this thread.
pub(super) unsafe fn send_frame(st: &mut ExchangeState, frame: Frame) -> Result<(), Closed> {
    let consumed = st.armed_at.elapsed();
    let (result, parked) = {
        let Some(tx) = st.job.ctx.sender.as_ref() else {
            return Err(Closed);
        };
        match tx.try_send(frame) {
            Ok(()) => (Ok(()), false),
            Err(TrySendError::Closed(_)) => (Err(Closed), false),
            Err(TrySendError::Full(frame)) => unsafe {
                // set_time_limit(0) means nothing is armed: skip the guard
                let saved = (*rapira_eg()).timeout_seconds;
                if saved > 0 {
                    zend_unset_timeout();
                }
                let r = park_send(tx, frame);
                if saved > 0 {
                    let remaining = (saved as u64).saturating_sub(consumed.as_secs()).max(1);
                    // the park loop is pure Rust - nothing between disarm and
                    // here can bailout or panic past the guard
                    zend_set_timeout(remaining as crate::zend_long, false);
                }
                (r, saved > 0)
            },
        }
    };
    if parked {
        st.armed_at = Instant::now();
    }
    result
}

/// The park: spin briefly, then 100µs naps. Only `Closed` ends it - a slow
/// consumer is backpressure, not cancellation; the front's write timeout is
/// what turns a dead-slow client into `Closed`.
pub(super) fn park_send(tx: &Sender<Frame>, mut frame: Frame) -> Result<(), Closed> {
    let mut spins = 0u32;
    loop {
        match tx.try_send(frame) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Closed(_)) => return Err(Closed),
            Err(TrySendError::Full(f)) => {
                frame = f;
                if spins < 64 {
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(Duration::from_micros(100));
                }
                spins = spins.saturating_add(1);
            }
        }
    }
}

/// Emit the committed head (implicit `200` with no fields when none), once.
/// `finalizing_len` is the whole body length when this call also ends the
/// response and nothing streamed before - the computed one-shot framing; a
/// declared content-length always wins, and a bodiless response never gets a
/// synthesized one.
/// # Safety
/// As `send_frame`.
pub(super) unsafe fn emit_head(
    st: &mut ExchangeState,
    finalizing_len: Option<u64>,
) -> Result<(), Closed> {
    if st.head_sent {
        return Ok(());
    }
    let (status, headers, body_coded) = match st.pending.take() {
        Some(p) => (p.status, p.headers, p.body_coded),
        None => (200, Vec::new(), false),
    };
    if st.stage == Stage::Open {
        st.stage = Stage::HeadCommitted;
    }
    let content_length = if st.bodiless {
        st.declared_cl
    } else {
        st.declared_cl.or(finalizing_len)
    };
    st.head_sent = true;
    unsafe {
        send_frame(
            st,
            Frame::Head {
                head: ResponseHead { status, headers },
                content_length,
                bodiless: st.bodiless,
                body_coded,
            },
        )
    }
}

/// The host got there first: conclude the unit exactly once. Setting
/// `Stage::Finalized` here is what keeps `exchange_drop`/`reclaim_current`
/// from counting the unit a second time; `discarded` selects the exception
/// class at gate 2.
pub(super) fn discard_unit(st: &mut ExchangeState) {
    if st.stage == Stage::Finalized {
        return;
    }
    st.discarded = true;
    st.stage = Stage::Finalized;
    if let BodyState::Multipart { files, .. } = &mut st.body {
        for p in files {
            p.upload.file.unlink();
        }
    }
    update(|c| {
        if let Unit::Handling(p) = c.unit {
            c.unit = Unit::Sealed(p);
        }
    });
    sb_update(Event::Handled(true));
    // best effort - the channel is usually already closed
    if let Some(tx) = st.job.ctx.sender.take() {
        let _ = tx.try_send(Frame::End {
            trailers: Vec::new(),
            truncated: true,
        });
    }
}

/// # Safety
/// As `send_frame`.
pub(super) unsafe fn write_trailers_core(st: &mut ExchangeState, trailers: FieldLines) -> Verb {
    if st.host_closed() {
        discard_unit(st);
        return Verb::Discarded;
    }
    if st.stage == Stage::Finalized {
        return Verb::Finalized;
    }
    // Nothing here commits a head: trailers may only end a response whose
    // final head is already committed (flush/sendFile/writeBody all commit).
    if st.stage == Stage::Open {
        return Verb::HeadNotWritten;
    }
    // Trailers-only: the head is committed but unsent, and nothing streamed,
    // so the framing stays a real content-length instead of empty chunked.
    if unsafe { emit_head(st, Some(st.sent_body)) }.is_err() {
        discard_unit(st);
        return Verb::Discarded;
    }
    // a bodiless response has no trailer section either, by the body's rule;
    // validation already ran, protocol-independent
    let trailers = if st.bodiless { Vec::new() } else { trailers };
    unsafe {
        seal(st, /*truncated=*/ false, trailers)
    };
    Verb::Ok
}

/// # Safety
/// `job` from receive; `trailers` a live, ZPP-owned array; engine active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_write_trailers(
    job: *mut c_void,
    trailers: *mut HashTable,
) -> bool {
    guard(false, || unsafe {
        // gate 1, input-only: shape + wire bytes + the forbidden categories
        let flat = match walk_head_table(trailers) {
            Ok(flat) => flat,
            Err(_) => {
                zend::throw_value_error(
                    c"a trailer name or value is not representable on the wire",
                );
                return false;
            }
        };
        if flat.iter().any(|(n, _)| forbidden_trailer(n)) {
            drop(flat);
            zend::throw_value_error(
                c"the field may not travel in a trailer section: framing, routing, authentication, request modifiers, response controls and content format stay in the head",
            );
            return false;
        }
        let st = &mut *job.cast::<ExchangeState>();
        match write_trailers_core(st, flat) {
            Verb::Ok | Verb::Interim => true,
            v => {
                throw_verb(v);
                false
            }
        }
    })
}

/// # Safety
/// As `send_frame`.
pub(super) unsafe fn write_head_core(
    st: &mut ExchangeState,
    status: u16,
    headers: FieldLines,
) -> Verb {
    if st.host_closed() {
        discard_unit(st);
        return Verb::Discarded;
    }
    // Finalized implies a committed head, so one gate covers both - writeHead's
    // documented class for any post-commit call is HeadAlreadyWrittenError.
    if st.stage != Stage::Open {
        return Verb::HeadWritten;
    }
    // 101 ends the HTTP conversation and counts as a final head; the other
    // 1xx are interim: on the wire at once, repeatable, no framing fields.
    if status != 101 && (100..200).contains(&status) {
        let head = ResponseHead {
            status,
            headers: strip_framing(headers),
        };
        return match unsafe { send_frame(st, Frame::Interim(head)) } {
            Ok(()) => Verb::Interim,
            Err(Closed) => {
                discard_unit(st);
                Verb::Discarded
            }
        };
    }
    let split = match split_framing(headers) {
        Ok(split) => split,
        Err(msg) => return Verb::BadField(msg),
    };
    st.declared_cl = split.declared_cl;
    st.pending = Some(PendingHead {
        status,
        headers: split.headers,
        body_coded: split.body_coded,
    });
    // 1xx carries no body either (RFC 9112 §6.3), so a committed 101 drops
    // chunks like 204/304; the front rewrites a final 1xx to 502.
    if matches!(status, 204 | 304 | 101) {
        st.bodiless = true;
    }
    st.stage = Stage::HeadCommitted;
    Verb::Ok
}

/// # Safety
/// `job` from receive; `headers` NULL or a live, ZPP-owned array.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_write_head(
    job: *mut c_void,
    status: i64,
    headers: *mut HashTable,
) -> bool {
    guard(false, || unsafe {
        let st = &mut *job.cast::<ExchangeState>();
        if !(100..=599).contains(&status) {
            crate::zend_value_error(
                c"status must be between 100 and 599, %lld given".as_ptr(),
                status as std::ffi::c_longlong,
            );
            return false;
        }
        let flat = match walk_head_table(headers) {
            Ok(flat) => flat,
            Err(msg) => {
                zend::throw_value_error(msg);
                return false;
            }
        };
        match write_head_core(st, status as u16, flat) {
            Verb::Ok | Verb::Interim => true,
            v => {
                throw_verb(v);
                false
            }
        }
    })
}

/// # Safety
/// `st` valid; `p` points at `len` readable bytes (checked against the cap
/// before the slice is formed). Engine active (`send_frame`).
pub(super) unsafe fn write_body_core(
    st: &mut ExchangeState,
    p: *const c_char,
    len: usize,
    eos: bool,
) -> Verb {
    if st.host_closed() {
        discard_unit(st);
        return Verb::Discarded;
    }
    if st.stage == Stage::Finalized {
        return Verb::Finalized;
    }
    // Contract: an empty chunk without eos does nothing - it is how a
    // chunked body terminates, never a head commit.
    if len == 0 && !eos {
        return Verb::Ok;
    }
    // Per-chunk cap: a single write the host will not hold in flight. Seal
    // truncated so the unit concludes instead of wedging the next receive().
    if len > MAX_BUFFERED_BODY {
        tracing::error!(
            target: "rapira",
            "response chunk exceeds the host buffer cap ({len} > {MAX_BUFFERED_BODY} bytes); sealing truncated"
        );
        let _ = unsafe { emit_head(st, None) };
        unsafe {
            seal(st, /*truncated=*/ true, Vec::new())
        };
        return Verb::Overflow;
    }
    let len64 = len as u64;
    if let Some(cl) = st.declared_cl
        && st.sent_body + len64 > cl
    {
        // The prefix rule: send the bytes that fit the declaration, complete
        // the response per it, reject this write. Later writes land on
        // Finalized. The surplus never reaches the wire.
        let fit = usize::try_from(cl - st.sent_body).unwrap_or(usize::MAX);
        if unsafe { emit_head(st, Some(cl)) }.is_ok() && fit > 0 && !st.bodiless {
            let bytes =
                Bytes::copy_from_slice(unsafe { std::slice::from_raw_parts(p.cast::<u8>(), fit) });
            let _ = unsafe { send_frame(st, Frame::Chunk(bytes)) };
        }
        st.sent_body = cl;
        unsafe {
            seal(st, /*truncated=*/ false, Vec::new())
        };
        return Verb::ContentLengthExceeded;
    }
    // The head leaves with the first body write; a one-shot carries its
    // computed length, a stream leaves the framing to the front.
    let finalizing = (eos && st.sent_body == 0).then_some(len64);
    if unsafe { emit_head(st, finalizing) }.is_err() {
        discard_unit(st);
        return Verb::Discarded;
    }
    st.sent_body += len64;
    if len > 0 && !st.bodiless {
        let bytes =
            Bytes::copy_from_slice(unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) });
        if unsafe { send_frame(st, Frame::Chunk(bytes)) }.is_err() {
            discard_unit(st);
            return Verb::Discarded;
        }
    }
    if eos {
        unsafe {
            seal(st, /*truncated=*/ false, Vec::new())
        };
    }
    Verb::Ok
}

/// # Safety
/// `job` from receive; `p` points at `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_write_body(
    job: *mut c_void,
    p: *const c_char,
    len: usize,
    eos: bool,
) -> bool {
    guard(false, || unsafe {
        let st = &mut *job.cast::<ExchangeState>();
        match write_body_core(st, p, len, eos) {
            Verb::Ok | Verb::Interim => true,
            v => {
                throw_verb(v);
                false
            }
        }
    })
}

/// The worker finished: unlink spools, count once, emit `End`, close the
/// stream. A gone client here is not an error - the worker finalized first.
/// # Safety
/// As `send_frame`.
pub(super) unsafe fn seal(st: &mut ExchangeState, truncated: bool, trailers: FieldLines) {
    // Contract: spool files are gone when the exchange finalizes. unlink takes
    // the path, so the Drop net stays a no-op afterwards.
    if let BodyState::Multipart { files, .. } = &mut st.body {
        for p in files {
            p.upload.file.unlink();
        }
    }
    st.stage = Stage::Finalized;
    update(|c| {
        if let Unit::Handling(p) = c.unit {
            c.unit = Unit::Sealed(p);
        }
        c.served = true;
    });
    sb_update(Event::Handled(truncated));
    let _ = unsafe {
        send_frame(
            st,
            Frame::End {
                trailers,
                truncated,
            },
        )
    };
    // deterministic close: a consumer reading to None never waits on free_obj
    st.job.ctx.sender = None;
}

/// # Safety
/// `job` from receive; engine active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_flush(job: *mut c_void) -> bool {
    guard(false, || unsafe {
        let st = &mut *job.cast::<ExchangeState>();
        let v = if st.host_closed() {
            discard_unit(st);
            Verb::Discarded
        } else if st.stage == Stage::Finalized {
            Verb::Finalized
        } else {
            // commits an implicit 200 when no head was written; with per-write
            // flushing nothing else is pending, so a repeat flush is a no-op
            match emit_head(st, None) {
                Ok(()) => Verb::Ok,
                Err(Closed) => {
                    discard_unit(st);
                    Verb::Discarded
                }
            }
        };
        match v {
            Verb::Ok | Verb::Interim => true,
            v => {
                throw_verb(v);
                false
            }
        }
    })
}

/// # Safety
/// `job` from receive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_is_finalized(job: *const c_void) -> bool {
    // false on a caught panic: claiming "ended" on an open unit would steer a
    // conforming worker into dropping it.
    guard(false, || unsafe {
        let st = &*job.cast::<ExchangeState>();
        // a host-closed unit reports finalized: the outcome is committed by
        // the host (read-only probe, no bookkeeping side effect)
        st.stage == Stage::Finalized || st.job.ctx.sender.as_ref().is_some_and(Sender::is_closed)
    })
}

/// # Safety
/// `job` from receive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_is_cancelled(job: *const c_void) -> bool {
    // false on a caught panic: a panic must not steer a conforming worker
    // into abandoning a healthy unit.
    guard(false, || unsafe {
        let st = &*job.cast::<ExchangeState>();
        st.host_closed()
    })
}

/// Reclaims the Box when PHP frees the Exchange object (free_obj). A unit
/// dropped unfinalized is failed by the host: a complete 500 when nothing
/// reached the wire, a truncated end otherwise - never an implicit response.
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
        let mut st = unsafe { Box::from_raw(ptr) };
        // A unit dying with the cycle (bailout: fatal, timeout) is a worker
        // death, not an abandonment - destructors are skipped on fatals per the
        // contract, so the loss goes to the host's deadline: the channel dies
        // unsent and the front reports the worker death.
        let cycle_died = unsafe { (*crate::rapira_cg()).unclean_shutdown };
        if st.stage != Stage::Finalized {
            sb_update(Event::Handled(true));
        }
        if st.stage != Stage::Finalized && !cycle_died {
            // same order as seal(): spool files are gone before the stream ends
            if let BodyState::Multipart { files, .. } = &mut st.body {
                for p in files {
                    p.upload.file.unlink();
                }
            }
            // best effort like discard_unit: a full or closed channel falls
            // back to channel-death semantics at the consumer
            if let Some(tx) = st.job.ctx.sender.take() {
                if st.head_sent {
                    let _ = tx.try_send(Frame::End {
                        trailers: Vec::new(),
                        truncated: true,
                    });
                } else if tx
                    .try_send(Frame::Head {
                        head: ResponseHead {
                            status: 500,
                            headers: Vec::new(),
                        },
                        content_length: (!st.bodiless).then_some(0),
                        bodiless: st.bodiless,
                        body_coded: false,
                    })
                    .is_ok()
                {
                    let _ = tx.try_send(Frame::End {
                        trailers: Vec::new(),
                        truncated: false,
                    });
                }
            }
        }
        drop(st);
    })
}
