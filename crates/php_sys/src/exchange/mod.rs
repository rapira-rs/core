//! The Rust half of `Rapira\Internal\Http\Exchange`: owns the `Job` while PHP
//! holds the unit, builds the `Rapira\Http\Request` graph, and runs the
//! response verbs as a frame stream (Interim/Head/Chunk/File/End) over the
//! job's sender. The graph builders follow the zend.rs frame rules.

pub(crate) use std::{
    cell::Cell,
    ffi::{CStr, CString, c_char, c_int, c_void},
    io::Read,
    path::Path,
    time::{Duration, Instant},
};

pub(crate) use bytes::Bytes;
pub(crate) use tokio::sync::mpsc::{Sender, error::TrySendError};

pub(crate) use crate::{
    HashPosition, HashTable, IS_ARRAY, IS_STRING, RAPIRA_MODE_DISPATCHER, add_assoc_zval_ex,
    add_next_index_object,
    callbacks::{MAX_BUFFERED_BODY, guard, is_field_value_byte, is_tchar},
    object_init_ex, rapira_array_init, rapira_ce_already_finalized_error,
    rapira_ce_closed_exception, rapira_ce_http_content_length_exceeded_error,
    rapira_ce_http_file_not_sendable_exception, rapira_ce_http_form_field,
    rapira_ce_http_head_already_written_error, rapira_ce_http_head_not_written_error,
    rapira_ce_http_multipart, rapira_ce_http_request, rapira_ce_http_tls,
    rapira_ce_http_uploaded_file, rapira_ce_inet_address, rapira_ce_internal_http_dispatcher,
    rapira_ce_internal_http_dispatcher_info, rapira_ce_internal_http_exchange,
    rapira_ce_not_in_dispatcher_mode_error, rapira_ce_timeout_exception, rapira_ce_unix_address,
    rapira_ce_work_discarded_exception, rapira_dispatcher_info_obj, rapira_eg, rapira_exchange_obj,
    rapira_receive_timed, rapira_receive_untimed,
    scoreboard::{Event, sb_update},
    start::{Pulled, pending_depth, pull_job_try, pull_job_wait},
    types::{Addr, Body, FieldLines, FormField, Frame, Job, ResponseHead, TlsView, UploadedFile},
    zend, zend_class_entry, zend_hash_get_current_data_ex, zend_hash_get_current_key_ex,
    zend_hash_internal_pointer_reset_ex, zend_hash_move_forward_ex, zend_object, zend_set_timeout,
    zend_string, zend_unset_timeout, zval, zval_add_ref, zval_ptr_dtor,
};

mod headers;
mod receive;
mod request;
mod respond;
mod sendfile;
#[cfg(test)]
mod tests;

pub use sendfile::set_sendfile_root;

/// The unit machine. Both live variants carry the Box pointer: free_obj
/// normally reclaims it, and tracking it here covers the paths where free_obj
/// cannot run (a bailed php_request_shutdown, a bailout between Box::into_raw
/// and the C-side store) - a leaked unfinalized unit would hang its client
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
    /// A unit was handed out this cycle; a fatal after that is an app failure,
    /// not a boot failure.
    received: bool,
}

const CYCLE_IDLE: CycleState = CycleState {
    unit: Unit::Idle,
    closed_seen: false,
    served: false,
    received: false,
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

// Worker-mode latch writers: the worker pull/serve path feeds the same
// per-cycle state the dispatcher verbs write, so run_cycle's classifier
// covers both resident modes.
pub(crate) fn note_closed() {
    update(|c| c.closed_seen = true);
}

pub(crate) fn note_received() {
    update(|c| c.received = true);
}

pub(crate) fn note_served() {
    update(|c| c.served = true);
}

pub(crate) fn served_any() -> bool {
    CYCLE.get().served
}

pub(crate) fn received_any() -> bool {
    CYCLE.get().received
}

/// Response progress. The head locks on the first head OR body write (per the
/// contract, a body chunk commits an implicit 200 first). Finalized is set by
/// seal() (worker finished) or discard_unit() (host got there first; the
/// `discarded` latch tells them apart).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Open,
    HeadCommitted,
    Finalized,
}

/// A committed final head, not yet on the wire - committing is not sending;
/// the bytes leave with the first body-touching verb.
struct PendingHead {
    status: u16,
    headers: FieldLines,
    body_coded: bool,
}

/// Request body as the unit holds it. Multipart parts carry their own derived
/// data, so nothing is index-aligned across parallel vectors.
enum BodyState {
    Raw(Vec<u8>),
    /// Host-parsed; spool files unlink at seal(), Drop is the abnormal-path net.
    Multipart {
        fields: Vec<FieldPart>,
        files: Vec<FilePart>,
    },
}

struct FieldPart {
    field: FormField,
    headers: Grouped,
}

struct FilePart {
    upload: UploadedFile,
    /// Rendered once: PathBuf bytes are not directly borrowable portably.
    path: Vec<u8>,
    headers: Grouped,
}

/// One endpoint, rendered at construction so the builder frame holds no owned
/// allocations of its own (zend.rs frame rule).
enum AddrOwned {
    Inet {
        ip: String,
        port: u16,
    },
    /// None = unnamed endpoint.
    Unix(Option<Vec<u8>>),
}

fn path_bytes(p: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    p.as_os_str().as_bytes().to_vec()
}

impl AddrOwned {
    fn new(a: &Addr) -> Self {
        match a {
            Addr::Inet(sa) => Self::Inet {
                ip: sa.ip().to_string(),
                port: sa.port(),
            },
            // an empty path is not a name: normalize to the unnamed endpoint
            Addr::Unix(p) => Self::Unix(p.as_deref().map(path_bytes).filter(|b| !b.is_empty())),
        }
    }
}

/// Field lines grouped name -> values, wire order, byte-exact names
/// (case-insensitive lookup is the consumer's job). Computed at construction
/// so builder frames only borrow (zend.rs frame rule). Keys are CStrings: the
/// symtable prefilter compiled into add_assoc_zval_ex reads one byte past a
/// leading `-`, which the terminator covers. An empty name (the contract key
/// type is non-empty-string) or one with an interior NUL (not a tchar) is
/// skipped.
struct Grouped(Vec<(CString, Vec<Vec<u8>>)>);

impl Grouped {
    fn new(headers: &[(String, Vec<u8>)]) -> Self {
        let mut out: Vec<(CString, Vec<Vec<u8>>)> = Vec::new();
        for (name, value) in headers {
            let nb = name.as_bytes();
            if nb.is_empty() {
                continue;
            }
            match out.iter_mut().find(|(n, _)| n.as_bytes() == nb) {
                Some((_, values)) => values.push(value.clone()),
                None => {
                    let Ok(key) = CString::new(nb) else { continue };
                    out.push((key, vec![value.clone()]));
                }
            }
        }
        Self(out)
    }
}

pub struct ExchangeState {
    // body above job: fields drop in declaration order, so an abandoned unit
    // unlinks its spool files (BodyState -> SpooledFile::drop) before the
    // frame sender closes - the file is gone before the stream ends, the same
    // order seal() guarantees.
    body: BodyState,
    job: Box<Job>,
    /// `Request::$headers`, pre-grouped.
    headers: Grouped,
    /// Absolute-form URI synthesized for `Request::$uri`.
    uri_abs: String,
    /// `Request::$target` bytes: the raw request-target, falling back to `uri`.
    target: Vec<u8>,
    /// `Request::$authority` bytes, byte-for-byte; None = named none.
    authority: Option<Vec<u8>>,
    /// Contract spelling for `Request::$protocol` (HTTP/2, not the CGI
    /// HTTP/2.0); everything unmapped passes through verbatim.
    protocol_php: String,
    remote: AddrOwned,
    server: AddrOwned,
    stage: Stage,
    /// The Head frame left for the channel.
    head_sent: bool,
    pending: Option<PendingHead>,
    /// A head-declared content-length, honoured then enforced.
    declared_cl: Option<u64>,
    /// Bytes accepted toward `declared_cl` - bodiless units count too (a HEAD
    /// handler shares the GET code path, errors included).
    sent_body: u64,
    /// The host closed the exchange first; sticky, selects the exception class.
    discarded: bool,
    /// 204 | 304 | a HEAD request | 101: chunks are accepted and dropped here.
    bodiless: bool,
    /// Last wall-timer arm; the park guard re-arms the remaining budget.
    armed_at: Instant,
}

impl ExchangeState {
    /// Err hands the job back with its sender intact; the caller fails the unit.
    fn new(mut job: Box<Job>) -> Result<Self, Box<Job>> {
        let taken = std::mem::replace(&mut job.ctx.req.body, Body::Raw(Box::new(std::io::empty())));
        let body = match taken {
            Body::Raw(mut reader) => {
                let mut buf = Vec::new();
                if let Err(e) = reader.read_to_end(&mut buf) {
                    tracing::error!(
                        target: "rapira",
                        "request body read failed for {} {}: {e}",
                        job.ctx.req.method, job.ctx.req.uri
                    );
                    job.ctx.req.body = Body::Raw(Box::new(std::io::empty()));
                    return Err(job);
                }
                BodyState::Raw(buf)
            }
            Body::Multipart(mb) => BodyState::Multipart {
                fields: mb
                    .fields
                    .into_iter()
                    .map(|f| FieldPart {
                        headers: Grouped::new(&f.headers),
                        field: f,
                    })
                    .collect(),
                files: mb
                    .files
                    .into_iter()
                    .map(|f| FilePart {
                        path: path_bytes(&f.file.path),
                        headers: Grouped::new(&f.headers),
                        upload: f,
                    })
                    .collect(),
            },
        };

        let req = &job.ctx.req;
        let headers = Grouped::new(&req.headers);
        // the producer normalized empty to None (runtime to_request)
        let authority = req.authority.clone();
        let target = req
            .target
            .clone()
            .unwrap_or_else(|| req.uri.clone().into_bytes());
        let protocol_php = match req.protocol.as_str() {
            "HTTP/2.0" => "HTTP/2".to_owned(),
            "HTTP/3.0" => "HTTP/3".to_owned(),
            p => p.to_owned(),
        };
        let remote = AddrOwned::new(&req.remote);
        let server = AddrOwned::new(&req.server);

        let scheme = if req.https { "https" } else { "http" };
        // $uri's authority: what the client named, else the listener address
        // (SocketAddr's Display brackets IPv6), else the configured name for a
        // unix listener, which has no host:port form of its own.
        let host = match &authority {
            Some(a) => String::from_utf8_lossy(a).into_owned(),
            None => match &req.server {
                Addr::Inet(sa) => sa.to_string(),
                Addr::Unix(_) => format!("{}:{}", req.server_name, req.server_port),
            },
        };
        // Asterisk-form (`OPTIONS *`) and CONNECT authority-form targets are not
        // paths; the contract has $uri fall back to the authority root there.
        let path = if req.uri.starts_with('/') {
            req.uri.as_str()
        } else {
            "/"
        };
        let uri_abs = format!("{scheme}://{host}{path}");

        let bodiless = req.method.eq_ignore_ascii_case("HEAD");

        Ok(Self {
            job,
            body,
            headers,
            uri_abs,
            target,
            authority,
            protocol_php,
            remote,
            server,
            stage: Stage::Open,
            head_sent: false,
            pending: None,
            declared_cl: None,
            sent_body: 0,
            discarded: false,
            bodiless,
            armed_at: Instant::now(),
        })
    }

    /// The host is gone (client, deadline, drain) and the worker has not
    /// finalized: the pre-write probe of gate 2.
    fn host_closed(&self) -> bool {
        self.discarded
            || (self.stage != Stage::Finalized
                && self.job.ctx.sender.as_ref().is_some_and(Sender::is_closed))
    }
}

// ---- the Request graph builder (zend.rs frame rules: zvals and raw pointers
// only, all bytes borrowed from ExchangeState)

/// The enclosing C layout from the engine's zend_object pointer (the C fields
/// sit before `std`; see the wrapper.h diagram).
unsafe fn exchange_from(obj: *mut zend_object) -> *mut rapira_exchange_obj {
    unsafe {
        obj.byte_sub(std::mem::offset_of!(rapira_exchange_obj, std))
            .cast()
    }
}

unsafe fn info_from(obj: *mut zend_object) -> *mut rapira_dispatcher_info_obj {
    unsafe {
        obj.byte_sub(std::mem::offset_of!(rapira_dispatcher_info_obj, std))
            .cast()
    }
}
