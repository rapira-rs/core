//! The Rust half of `Rapira\Internal\Http\Exchange`: owns the `Job` while PHP
//! holds the unit, builds the `Rapira\Http\Request` graph, and runs the
//! response verbs as a frame stream (Interim/Head/Chunk/File/End) over the
//! job's sender. The graph builders follow the zend.rs frame rules.

use std::{
    cell::Cell,
    ffi::{CStr, CString, c_char, c_int, c_void},
    io::Read,
    path::Path,
    time::{Duration, Instant},
};

use bytes::Bytes;
use tokio::sync::mpsc::{Sender, error::TrySendError};

use crate::{
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
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        p.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        p.to_string_lossy().into_owned().into_bytes()
    }
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

/// A `Grouped` into the `array<non-empty-string, list<string>>` shape.
/// Symtable key normalization (an all-digit name lands as an integer key, or
/// the array disagrees with every userland lookup of it) is add_assoc_zval_ex's
/// own zend_symtable_str_update.
unsafe fn emit_headers(dst: *mut zval, g: &Grouped) {
    unsafe {
        rapira_array_init(dst, g.0.len() as u32);
        for (name, values) in &g.0 {
            let mut list: zval = std::mem::zeroed();
            rapira_array_init(&mut list, values.len() as u32);
            for v in values {
                zend::list_push_stringl(&mut list, v);
            }
            // moved out: the hash-update family never addrefs
            add_assoc_zval_ex(dst, name.as_ptr(), name.count_bytes(), &mut list);
        }
    }
}

unsafe fn build_address(dst: *mut zval, addr: &AddrOwned) {
    unsafe {
        match addr {
            AddrOwned::Inet { ip, port } => {
                let ce = rapira_ce_inet_address;
                let _ = object_init_ex(dst, ce);
                let o = (*dst).value.obj;
                zend::prop_stringl(ce, o, c"ip", ip.as_bytes());
                zend::prop_long(ce, o, c"port", i64::from(*port));
            }
            AddrOwned::Unix(path) => {
                let ce = rapira_ce_unix_address;
                let _ = object_init_ex(dst, ce);
                zend::prop_str_or_null(ce, (*dst).value.obj, c"path", path.as_deref());
            }
        }
    }
}

unsafe fn build_tls(dst: *mut zval, t: &TlsView) {
    unsafe {
        let ce = rapira_ce_http_tls;
        let _ = object_init_ex(dst, ce);
        let o = (*dst).value.obj;
        zend::prop_stringl(ce, o, c"version", t.version.as_bytes());
        zend::prop_stringl(ce, o, c"cipher", t.cipher.as_bytes());
        zend::prop_str_or_null(
            ce,
            o,
            c"negotiatedProtocol",
            t.alpn.as_deref().map(str::as_bytes),
        );
        zend::prop_str_or_null(
            ce,
            o,
            c"requestedServerName",
            t.server_name.as_deref().map(str::as_bytes),
        );
        match t.cert.as_ref() {
            Some(cert) => {
                zend::prop_stringl(ce, o, c"certSerial", cert.serial.as_bytes());
                zend::prop_str_or_null(
                    ce,
                    o,
                    c"certOrganization",
                    cert.organization.as_deref().map(str::as_bytes),
                );
                zend::prop_stringl(ce, o, c"certFingerprint", cert.fingerprint.as_bytes());
            }
            None => {
                zend::prop_null(ce, o, c"certSerial");
                zend::prop_null(ce, o, c"certOrganization");
                zend::prop_null(ce, o, c"certFingerprint");
            }
        }
    }
}

unsafe fn build_file(dst: *mut zval, p: &FilePart) {
    unsafe {
        let ce = rapira_ce_http_uploaded_file;
        let _ = object_init_ex(dst, ce);
        let o = (*dst).value.obj;
        zend::prop_stringl(ce, o, c"name", &p.upload.name);
        zend::prop_stringl(ce, o, c"clientFilename", &p.upload.client_filename);
        zend::prop_str_or_null(
            ce,
            o,
            c"clientMediaType",
            p.upload.client_media_type.as_deref(),
        );
        let mut headers: zval = std::mem::zeroed();
        emit_headers(&mut headers, &p.headers);
        zend::prop_zval(ce, o, c"headers", &mut headers);
        zval_ptr_dtor(&mut headers);
        zend::prop_stringl(ce, o, c"tmpPath", &p.path);
        // a 64-bit zend_long is assumed (NTS targets are LP64)
        zend::prop_long(ce, o, c"size", p.upload.size as i64);
    }
}

/// Multipart{fields, files} into `dst`. A throw at any hand-off stops the
/// loops - property writes with a pending exception are what the checkpoints
/// exist to prevent; a partial graph is released and never cached.
unsafe fn build_multipart(
    dst: *mut zval,
    field_parts: &[FieldPart],
    file_parts: &[FilePart],
) -> bool {
    unsafe {
        let mut fields: zval = std::mem::zeroed();
        rapira_array_init(&mut fields, field_parts.len() as u32);
        let mut files: zval = std::mem::zeroed();
        rapira_array_init(&mut files, file_parts.len() as u32);

        for p in field_parts {
            let ce = rapira_ce_http_form_field;
            let mut part: zval = std::mem::zeroed();
            let _ = object_init_ex(&mut part, ce);
            let o = part.value.obj;
            zend::prop_stringl(ce, o, c"name", &p.field.name);
            zend::prop_stringl(ce, o, c"value", &p.field.value);
            let mut headers: zval = std::mem::zeroed();
            emit_headers(&mut headers, &p.headers);
            zend::prop_zval(ce, o, c"headers", &mut headers);
            zval_ptr_dtor(&mut headers);
            if zend::exception_pending() {
                zval_ptr_dtor(&mut part);
                zval_ptr_dtor(&mut fields);
                zval_ptr_dtor(&mut files);
                return false;
            }
            let _ = add_next_index_object(&mut fields, o); // the ref moves in
        }

        for p in file_parts {
            let mut part: zval = std::mem::zeroed();
            build_file(&mut part, p);
            if zend::exception_pending() {
                zval_ptr_dtor(&mut part);
                zval_ptr_dtor(&mut fields);
                zval_ptr_dtor(&mut files);
                return false;
            }
            let _ = add_next_index_object(&mut files, part.value.obj); // the ref moves in
        }

        let ce = rapira_ce_http_multipart;
        let _ = object_init_ex(dst, ce);
        let o = (*dst).value.obj;
        zend::prop_zval(ce, o, c"fields", &mut fields);
        zend::prop_zval(ce, o, c"files", &mut files);
        zval_ptr_dtor(&mut fields);
        zval_ptr_dtor(&mut files);
        if zend::exception_pending() {
            zval_ptr_dtor(dst);
            return false;
        }
        true
    }
}

/// Builds `Rapira\Http\Request` into `return_value`, memoizing on the
/// exchange. Contract with the C shell: false means a throw is pending - a
/// caught panic returns false without one and the shell backstops with its
/// own zend_throw_error.
/// # Safety
/// `ex` a live exchange with a non-null job (the shell checks); `return_value`
/// writable. Frame rules: zend.rs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_build_request(
    ex: *mut rapira_exchange_obj,
    return_value: *mut zval,
) -> bool {
    guard(false, || unsafe { build_request_impl(ex, return_value) })
}

unsafe fn build_request_impl(ex: *mut rapira_exchange_obj, return_value: *mut zval) -> bool {
    unsafe {
        if !zend::is_undef(&(*ex).request) {
            *return_value = (*ex).request;
            zval_add_ref(return_value);
            return true;
        }
        let ce: *mut zend_class_entry = rapira_ce_http_request;
        if ce.is_null() {
            // pre-MINIT call: unreachable by construction; the shell throws
            return false;
        }
        let st = &*(*ex).job.cast::<ExchangeState>();
        let req = &st.job.ctx.req;

        let mut headers: zval = std::mem::zeroed();
        emit_headers(&mut headers, &st.headers);
        let mut remote: zval = std::mem::zeroed();
        build_address(&mut remote, &st.remote);
        let mut server: zval = std::mem::zeroed();
        build_address(&mut server, &st.server);
        // stays IS_UNDEF when absent; every cleanup dtors it unconditionally
        // (a no-op on undef)
        let mut tls: zval = std::mem::zeroed();
        if let Some(t) = req.tls.as_ref() {
            build_tls(&mut tls, t);
        }
        let mut mp: zval = std::mem::zeroed();
        if let BodyState::Multipart { fields, files } = &st.body
            && !build_multipart(&mut mp, fields, files)
        {
            zval_ptr_dtor(&mut headers);
            zval_ptr_dtor(&mut remote);
            zval_ptr_dtor(&mut server);
            zval_ptr_dtor(&mut tls);
            return false;
        }
        // never assemble on top of a throw
        if zend::exception_pending() {
            zval_ptr_dtor(&mut headers);
            zval_ptr_dtor(&mut remote);
            zval_ptr_dtor(&mut server);
            zval_ptr_dtor(&mut tls);
            zval_ptr_dtor(&mut mp);
            return false;
        }

        let mut reqz: zval = std::mem::zeroed();
        let _ = object_init_ex(&mut reqz, ce);
        let o = reqz.value.obj;
        zend::prop_stringl(ce, o, c"method", req.method.as_bytes());
        zend::prop_stringl(ce, o, c"uri", st.uri_abs.as_bytes());
        zend::prop_stringl(ce, o, c"target", &st.target);
        zend::prop_str_or_null(ce, o, c"authority", st.authority.as_deref());
        zend::prop_stringl(ce, o, c"protocol", st.protocol_php.as_bytes());
        zend::prop_zval(ce, o, c"headers", &mut headers);
        zval_ptr_dtor(&mut headers);
        // the union slot takes either arm through the same handler
        match &st.body {
            BodyState::Raw(v) => zend::prop_stringl(ce, o, c"body", v),
            BodyState::Multipart { .. } => {
                zend::prop_zval(ce, o, c"body", &mut mp);
                zval_ptr_dtor(&mut mp);
            }
        }
        zend::prop_zval(ce, o, c"remote", &mut remote);
        zval_ptr_dtor(&mut remote);
        zend::prop_zval(ce, o, c"server", &mut server);
        zval_ptr_dtor(&mut server);
        if req.tls.is_some() {
            zend::prop_zval(ce, o, c"tls", &mut tls);
        } else {
            zend::prop_null(ce, o, c"tls");
        }
        zval_ptr_dtor(&mut tls);
        zend::prop_double(ce, o, c"receivedAt", req.received_at.unwrap_or(0.0));

        // a throw during the writes leaves uninitialized readonly slots:
        // release the partial graph and leave the memo unset, or every later
        // getRequest() would hand back the poisoned object
        if zend::exception_pending() {
            zval_ptr_dtor(&mut reqz);
            return false;
        }
        (*ex).request = reqz; // the memo takes the ref
        *return_value = reqz;
        zval_add_ref(return_value); // the caller's ref
        true
    }
}

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

enum RecvMode {
    Wait(i64),
    Try,
}

/// Pull a unit into a fresh Exchange object in `return_value`, or throw.
/// # Safety
/// `return_value` writable; engine active on this thread.
unsafe fn receive_into(return_value: *mut zval, mode: RecvMode) -> bool {
    unsafe {
        // busy check before the disarm: the handling unit keeps its budget
        if matches!(CYCLE.get().unit, Unit::Handling(_)) {
            zend::throw_error(
                c"receive() while a Rapira\\Http\\Exchange is unfinalized; finalize it first",
            );
            return false;
        }
        // The wrapping object exists before the pull: zend_object_alloc can
        // bailout, and a Box already pulled with no owner would leak its Frame
        // sender and hang the client. With the object first, the error path is
        // a plain dtor and free_obj no-ops on the NULL job slot.
        let mut obj: zval = std::mem::zeroed();
        let _ = object_init_ex(&mut obj, rapira_ce_internal_http_exchange);
        // Timeout/Empty/Closed stay untimed until the next receive verb: the
        // worker is between units, not executing one.
        // SAFETY: plain zend timer bookkeeping on this thread; no bailout path.
        rapira_receive_untimed();
        loop {
            let pulled = match mode {
                RecvMode::Try | RecvMode::Wait(0) => pull_job_try(),
                RecvMode::Wait(-1) => pull_job_wait(None),
                RecvMode::Wait(t) => pull_job_wait(Some(Duration::from_micros(t as u64))),
            };
            match pulled {
                Pulled::Job(job) => {
                    let st = match ExchangeState::new(job) {
                        Ok(st) => st,
                        // the unit could not be materialized and was failed
                        // upstream; wait for the next one
                        Err(mut job) => {
                            job.ctx.finish(true);
                            sb_update(Event::Handled(true));
                            continue;
                        }
                    };
                    // a client that vanished while queued: fail the unit
                    // instead of handing it out (drop unlinks any spools)
                    if st.job.ctx.sender.as_ref().is_some_and(Sender::is_closed) {
                        sb_update(Event::Handled(true));
                        continue;
                    }
                    let ptr = Box::into_raw(Box::new(st));
                    // a previous Sealed unit becomes free_obj's sole
                    // responsibility: its frame is already delivered, so worst
                    // case is a leak, not a hang
                    update(|c| {
                        c.unit = Unit::Handling(ptr);
                        c.received = true;
                    });
                    (*exchange_from(obj.value.obj)).job = ptr.cast();
                    // Arm the captured budget only once the unit is owned and
                    // handed out; from here PHP is executing on the clock.
                    // SAFETY: plain zend timer bookkeeping; no bailout path.
                    rapira_receive_timed();
                    (*ptr).armed_at = Instant::now();
                    // the object's ref moves to return_value
                    *return_value = obj;
                    return true;
                }
                Pulled::Closed => {
                    update(|c| c.closed_seen = true);
                    zval_ptr_dtor(&mut obj);
                    zend::throw_exception(
                        rapira_ce_closed_exception,
                        c"no more work will ever arrive",
                    );
                    return false;
                }
                Pulled::Empty if matches!(mode, RecvMode::Try) => {
                    zval_ptr_dtor(&mut obj);
                    zend::zval_null(return_value);
                    return true;
                }
                Pulled::Timeout | Pulled::Empty => {
                    zval_ptr_dtor(&mut obj);
                    zend::throw_exception(
                        rapira_ce_timeout_exception,
                        c"no work became available within the timeout",
                    );
                    return false;
                }
            }
        }
    }
}

/// # Safety
/// `return_value` writable; engine active on this thread (the receive verbs
/// touch the zend timer).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_receive(timeout_us: i64, return_value: *mut zval) -> bool {
    guard(false, || unsafe {
        if timeout_us < -1 {
            crate::zend_argument_value_error(1, c"must be greater than or equal to -1".as_ptr());
            return false;
        }
        receive_into(return_value, RecvMode::Wait(timeout_us))
    })
}

/// # Safety
/// As `rapira_rs_receive`; never blocks. Empty writes null instead of throwing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_try_receive(return_value: *mut zval) -> bool {
    guard(false, || unsafe {
        receive_into(return_value, RecvMode::Try)
    })
}

/// Builds the DispatcherInfo snapshot into `return_value`.
/// # Safety
/// `return_value` writable; engine active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_dispatcher_info(return_value: *mut zval) -> bool {
    guard(false, || unsafe {
        let _ = object_init_ex(return_value, rapira_ce_internal_http_dispatcher_info);
        let info = info_from((*return_value).value.obj);
        (*info).pending = pending_depth() as i64;
        (*info).active = i64::from(matches!(CYCLE.get().unit, Unit::Handling(_)));
        true
    })
}

thread_local! {
    /// The per-thread `get_dispatcher()` singleton; released at RSHUTDOWN.
    static DISPATCHER: Cell<Option<zval>> = const { Cell::new(None) };
}

/// # Safety
/// `return_value` writable; engine active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_get_dispatcher(return_value: *mut zval) -> bool {
    guard(false, || unsafe {
        if crate::rapira_mode != RAPIRA_MODE_DISPATCHER as c_int {
            zend::throw_exception(
                rapira_ce_not_in_dispatcher_mode_error,
                c"nothing dispatches work to this process outside dispatcher mode",
            );
            return false;
        }
        let inst = DISPATCHER.with(|d| match d.get() {
            Some(zv) => zv,
            None => {
                let mut zv: zval = std::mem::zeroed();
                let _ = object_init_ex(&mut zv, rapira_ce_internal_http_dispatcher);
                d.set(Some(zv));
                zv
            }
        });
        // RETURN_COPY: the singleton keeps its ref, the caller gets its own
        *return_value = inst;
        zval_add_ref(return_value);
        true
    })
}

/// Called from the C RSHUTDOWN bracket.
#[unsafe(no_mangle)]
pub extern "C" fn rapira_rs_dispatcher_release() {
    guard((), || {
        DISPATCHER.with(|d| {
            if let Some(mut zv) = d.take() {
                // SAFETY: the zval came from object_init_ex on this thread.
                unsafe { zval_ptr_dtor(&mut zv) };
            }
        });
    })
}

/// Verb outcomes; only the non-Ok arms surface to PHP, as throws. The cores
/// return these instead of throwing so no owned state is live when
/// `zend_throw_*` (which can bailout) runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
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
unsafe fn throw_verb(v: Verb) {
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

struct Closed;

/// Push a frame; on a full channel, park with the wall timer disarmed. A
/// parked thread never reaches an opcode boundary, so a fired timeout could
/// not longjmp anyway - and on NTS its second expiry would `_exit(124)` the
/// process. The re-arm grants the remaining budget (floor 1s), so
/// max_execution_time keeps bounding compute while park time is excluded.
/// # Safety
/// Engine active on this thread.
unsafe fn send_frame(st: &mut ExchangeState, frame: Frame) -> Result<(), Closed> {
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
fn park_send(tx: &Sender<Frame>, mut frame: Frame) -> Result<(), Closed> {
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
unsafe fn emit_head(st: &mut ExchangeState, finalizing_len: Option<u64>) -> Result<(), Closed> {
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
fn discard_unit(st: &mut ExchangeState) {
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

/// The root `sendFile()` paths must stay inside; canonicalized at set. None =
/// deny (the binary sets it at boot; tests set what they need).
static SENDFILE_ROOT: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

pub fn set_sendfile_root(root: std::path::PathBuf) {
    let canonical = std::fs::canonicalize(&root).unwrap_or_else(|e| {
        // fail closed but not silently: a raw root never matches the
        // canonicalized candidate, so every sendFile() will be rejected
        tracing::warn!(
            target: "rapira",
            "sendfile root {} cannot be canonicalized ({e}); sendFile() will reject every path",
            root.display()
        );
        root
    });
    *SENDFILE_ROOT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(canonical);
}

fn sendfile_root() -> Option<std::path::PathBuf> {
    SENDFILE_ROOT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The path the kernel holds for the open descriptor. None = no readback
/// (or the file was unlinked meanwhile); callers fail closed.
#[cfg(target_os = "macos")]
fn fd_path(file: &std::fs::File) -> Option<std::path::PathBuf> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;
    unsafe extern "C" {
        // variadic on purpose: Darwin aarch64 passes variadic args differently,
        // so a fixed-arity declaration would be an ABI mismatch
        fn fcntl(fd: std::os::raw::c_int, cmd: std::os::raw::c_int, ...) -> std::os::raw::c_int;
    }
    // Darwin fcntl.h: F_GETPATH = 50, buffer must hold MAXPATHLEN (1024)
    const F_GETPATH: std::os::raw::c_int = 50;
    let mut buf = [0u8; 1024];
    // SAFETY: F_GETPATH writes at most MAXPATHLEN bytes incl. NUL into buf
    if unsafe { fcntl(file.as_raw_fd(), F_GETPATH, buf.as_mut_ptr()) } == -1 {
        return None;
    }
    let len = buf.iter().position(|&b| b == 0)?;
    Some(std::path::PathBuf::from(std::ffi::OsString::from_vec(
        buf[..len].to_vec(),
    )))
}

/// See above; /proc works on Linux and FreeBSD-with-procfs, and a missing
/// /proc fails closed.
#[cfg(not(target_os = "macos"))]
fn fd_path(file: &std::fs::File) -> Option<std::path::PathBuf> {
    use std::os::fd::AsRawFd;
    let p = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()?;
    // an unlinked target reads back as "<path> (deleted)"
    (!p.as_os_str().as_encoded_bytes().ends_with(b" (deleted)")).then_some(p)
}

/// Gate 4b: open and validate the file on this thread, so the throw precedes
/// any write. Returns the opened file and the slice length.
fn open_send_file(
    path: &[u8],
    offset: u64,
    length: Option<u64>,
) -> Result<(std::fs::File, u64), &'static CStr> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(path));
    // canonicalize resolves symlinks, so a link out of the root is an escape
    let canonical = std::fs::canonicalize(path).map_err(|_| c"no readable file at the path")?;
    let Some(root) = sendfile_root() else {
        return Err(c"no sendfile root is configured");
    };
    if !canonical.starts_with(&root) {
        return Err(c"the path is outside the configured sendfile root");
    }
    let file = std::fs::File::open(&canonical).map_err(|_| c"no readable file at the path")?;
    // the canonicalize walk and the open are two independent traversals; a
    // symlink component swapped between them must fail closed, so re-check
    // containment on the path the kernel holds for the descriptor itself
    let held = fd_path(&file).ok_or(c"no readable file at the path")?;
    if !held.starts_with(&root) {
        return Err(c"the path is outside the configured sendfile root");
    }
    let meta = file
        .metadata()
        .map_err(|_| c"no readable file at the path")?;
    if !meta.is_file() {
        return Err(c"not a regular file");
    }
    let size = meta.len();
    if offset > size {
        return Err(c"the requested slice runs past the end of the file");
    }
    let len = match length {
        Some(l) => {
            if offset + l > size {
                return Err(c"the requested slice runs past the end of the file");
            }
            l
        }
        None => size - offset,
    };
    Ok((file, len))
}

/// # Safety
/// As `send_frame`.
unsafe fn send_file_core(
    st: &mut ExchangeState,
    path: &[u8],
    offset: u64,
    length: Option<u64>,
    eos: bool,
) -> Verb {
    if st.host_closed() {
        discard_unit(st);
        return Verb::Discarded;
    }
    if st.stage == Stage::Finalized {
        return Verb::Finalized;
    }
    let (file, len) = match open_send_file(path, offset, length) {
        Ok(opened) => opened,
        Err(msg) => return Verb::FileNotSendable(msg),
    };
    if let Some(cl) = st.declared_cl
        && st.sent_body + len > cl
    {
        // the prefix rule, sendFile flavour: the fitting sub-slice is sent
        let fit = cl - st.sent_body;
        if unsafe { emit_head(st, Some(cl)) }.is_ok() && fit > 0 && !st.bodiless {
            let _ = unsafe {
                send_frame(
                    st,
                    Frame::File {
                        file,
                        offset,
                        len: fit,
                    },
                )
            };
        }
        st.sent_body = cl;
        unsafe {
            seal(st, /*truncated=*/ false, Vec::new())
        };
        return Verb::ContentLengthExceeded;
    }
    // the host knows the length up front: an eos sendFile with nothing
    // streamed before carries a real content-length without buffering
    let finalizing = (eos && st.sent_body == 0).then_some(len);
    if unsafe { emit_head(st, finalizing) }.is_err() {
        discard_unit(st);
        return Verb::Discarded;
    }
    st.sent_body += len;
    if len > 0
        && !st.bodiless
        && unsafe { send_frame(st, Frame::File { file, offset, len }) }.is_err()
    {
        discard_unit(st);
        return Verb::Discarded;
    }
    if eos {
        unsafe {
            seal(st, /*truncated=*/ false, Vec::new())
        };
    }
    Verb::Ok
}

/// # Safety
/// `job` from receive; `path` points at `path_len` readable bytes (ZPP-owned);
/// engine active on this thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_send_file(
    job: *mut c_void,
    path: *const c_char,
    path_len: usize,
    offset: i64,
    length: i64,
    length_is_null: bool,
    eos: bool,
) -> bool {
    guard(false, || unsafe {
        if offset < 0 {
            crate::zend_argument_value_error(2, c"must be greater than or equal to 0".as_ptr());
            return false;
        }
        if !length_is_null && length < 1 {
            crate::zend_argument_value_error(3, c"must be greater than or equal to 1".as_ptr());
            return false;
        }
        let st = &mut *job.cast::<ExchangeState>();
        let path = std::slice::from_raw_parts(path.cast::<u8>(), path_len);
        let length = (!length_is_null).then_some(length as u64);
        match send_file_core(st, path, offset as u64, length, eos) {
            Verb::Ok | Verb::Interim => true,
            v => {
                throw_verb(v);
                false
            }
        }
    })
}

/// RFC 9110 §6.5.1's categories, materialized: fields that may not travel in
/// a trailer section (framing, connection family per §7.6.1, routing,
/// authentication, request modifiers, response controls, content format).
/// Unknown/extension fields pass.
/// https://www.rfc-editor.org/rfc/rfc9110#section-6.5.1
const TRAILER_FORBIDDEN: &[&str] = &[
    "age",
    "authorization",
    "cache-control",
    "connection",
    "content-encoding",
    "content-language",
    "content-length",
    "content-location",
    "content-range",
    "content-type",
    "cookie",
    "date",
    "expect",
    "expires",
    "forwarded",
    "host",
    "if-match",
    "if-modified-since",
    "if-none-match",
    "if-range",
    "if-unmodified-since",
    "keep-alive",
    "location",
    "max-forwards",
    "pragma",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "range",
    "retry-after",
    "set-cookie",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "vary",
    "via",
    "warning",
    "www-authenticate",
];

fn forbidden_trailer(name: &str) -> bool {
    TRAILER_FORBIDDEN
        .iter()
        .any(|f| name.eq_ignore_ascii_case(f))
}

/// # Safety
/// As `send_frame`.
unsafe fn write_trailers_core(st: &mut ExchangeState, trailers: FieldLines) -> Verb {
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

fn is_hop_by_hop(name: &str) -> bool {
    // RFC 9110 §7.6.1's remove set plus proxy-connection; `trailer` is not
    // hop-by-hop and stays. https://www.rfc-editor.org/rfc/rfc9110#section-7.6.1
    [
        "transfer-encoding",
        "connection",
        "keep-alive",
        "upgrade",
        "te",
        "proxy-connection",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

/// An interim head carries no framing fields at all.
fn strip_framing(mut headers: FieldLines) -> FieldLines {
    headers.retain(|(n, _)| !is_hop_by_hop(n) && !n.eq_ignore_ascii_case("content-length"));
    headers
}

/// A final head's fields after the framing post-filter.
struct SplitHead {
    headers: FieldLines,
    declared_cl: Option<u64>,
    body_coded: bool,
}

/// Final-head post-filter: the host frames the response, so hop-by-hop fields
/// drop silently; a content-length is extracted into the declared framing
/// (unparseable → dropped, repeated → `\ValueError`); content-encoding marks
/// the body as already coded.
fn split_framing(headers: FieldLines) -> Result<SplitHead, &'static CStr> {
    let mut declared_cl: Option<u64> = None;
    let mut cl_lines = 0usize;
    let mut body_coded = false;
    let mut out = Vec::with_capacity(headers.len());
    for (n, v) in headers {
        if n.eq_ignore_ascii_case("content-length") {
            cl_lines += 1;
            if cl_lines > 1 {
                return Err(c"content-length may not repeat");
            }
            declared_cl = parse_content_length(&v);
            continue;
        }
        if is_hop_by_hop(&n) {
            continue;
        }
        if n.eq_ignore_ascii_case("content-encoding") {
            body_coded = true;
        }
        out.push((n, v));
    }
    Ok(SplitHead {
        headers: out,
        declared_cl,
        body_coded,
    })
}

fn parse_content_length(v: &[u8]) -> Option<u64> {
    let s = std::str::from_utf8(v).ok()?.trim();
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// RFC 9110 §5.6.2 tchar. A non-token name would pass a weaker check and then
/// be dropped silently downstream instead of raising the promised ValueError.
/// https://www.rfc-editor.org/rfc/rfc9110#section-5.6.2
fn wire_token(name: &[u8]) -> bool {
    !name.is_empty() && name.iter().all(|&b| is_tchar(b))
}

/// Field values per RFC 9110 §5.5 - the same byte set the classic path
/// enforces, so a value the front would drop raises the ValueError here.
/// https://www.rfc-editor.org/rfc/rfc9110#section-5.5
fn wire_value(value: &[u8]) -> bool {
    value.iter().all(|&b| is_field_value_byte(b))
}

/// The `array<non-empty-string, list<string>>` shape into flat pairs, wire
/// validation included. Err carries the ValueError message.
/// # Safety
/// `ht` NULL or a live array; entries stay ZPP-owned.
unsafe fn walk_head_table(
    ht: *mut HashTable,
) -> Result<Vec<(String, Vec<u8>)>, &'static std::ffi::CStr> {
    let mut flat = Vec::new();
    if ht.is_null() {
        return Ok(flat);
    }
    unsafe {
        let mut pos: HashPosition = 0;
        zend_hash_internal_pointer_reset_ex(ht, &mut pos);
        loop {
            // raw pointer: the pos parameter is *mut on 8.4 and *const on 8.5
            let entry = zend_hash_get_current_data_ex(ht, &raw mut pos);
            if entry.is_null() {
                break;
            }
            let mut str_key: *mut zend_string = std::ptr::null_mut();
            let mut num_key = 0;
            let kt = zend_hash_get_current_key_ex(ht, &mut str_key, &mut num_key, &pos);
            if i64::from(kt) != crate::HASH_KEY_IS_STRING || str_key.is_null() {
                return Err(c"header name is not representable on the wire");
            }
            let name = zend::zstr_bytes(str_key);
            if !wire_token(name) {
                return Err(c"header name is not representable on the wire");
            }
            let list = zend::deref(entry);
            if zend::zval_type(list) != IS_ARRAY {
                return Err(c"each header entry must be a list of strings");
            }
            let inner = (*list).value.arr;
            let mut ipos: HashPosition = 0;
            zend_hash_internal_pointer_reset_ex(inner, &mut ipos);
            loop {
                let item = zend_hash_get_current_data_ex(inner, &raw mut ipos);
                if item.is_null() {
                    break;
                }
                let item = zend::deref(item);
                if zend::zval_type(item) != IS_STRING {
                    return Err(c"header value is not representable on the wire");
                }
                let value = zend::zstr_bytes((*item).value.str_);
                if !wire_value(value) {
                    return Err(c"header value is not representable on the wire");
                }
                flat.push((String::from_utf8_lossy(name).into_owned(), value.to_vec()));
                zend_hash_move_forward_ex(inner, &mut ipos);
            }
            zend_hash_move_forward_ex(ht, &mut pos);
        }
    }
    Ok(flat)
}

/// # Safety
/// As `send_frame`.
unsafe fn write_head_core(st: &mut ExchangeState, status: u16, headers: FieldLines) -> Verb {
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
unsafe fn write_body_core(st: &mut ExchangeState, p: *const c_char, len: usize, eos: bool) -> Verb {
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
unsafe fn seal(st: &mut ExchangeState, truncated: bool, trailers: FieldLines) {
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
        if st.stage != Stage::Finalized {
            sb_update(Event::Handled(true));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Context, Request};
    use std::path::PathBuf;

    fn base_req() -> Request {
        Request {
            method: String::new(),
            uri: "/".into(),
            target: None,
            authority: None,
            https: false,
            query: String::new(),
            protocol: String::new(),
            remote: Addr::Inet(([127, 0, 0, 1], 8080).into()),
            server: Addr::Inet(([127, 0, 0, 1], 8080).into()),
            server_name: String::new(),
            server_port: 8080,
            script_name: String::new(),
            document_root: String::new(),
            script_filename: PathBuf::new(),
            headers: Vec::new(),
            server_vars: Vec::new(),
            content_type: None,
            content_length: 0,
            body: Body::Raw(Box::new(std::io::empty())),
            received_at: None,
            tls: None,
        }
    }

    /// A sealed response stream, collected.
    enum Sealed {
        Complete { status: u16, body: Vec<u8> },
        Truncated { status: Option<u16>, body: Vec<u8> },
        Nothing,
    }

    fn recv_sealed(rx: &mut tokio::sync::mpsc::Receiver<crate::types::Frame>) -> Sealed {
        let (mut status, mut body, mut saw_frames) = (None, Vec::new(), false);
        while let Ok(frame) = rx.try_recv() {
            saw_frames = true;
            match frame {
                crate::types::Frame::Interim(_) | crate::types::Frame::File { .. } => {}
                crate::types::Frame::Head { head, .. } => status = Some(head.status),
                crate::types::Frame::Chunk(b) => body.extend_from_slice(&b),
                crate::types::Frame::End { truncated, .. } => {
                    return match (truncated, status) {
                        (true, status) => Sealed::Truncated { status, body },
                        (false, Some(status)) => Sealed::Complete { status, body },
                        (false, None) => Sealed::Nothing,
                    };
                }
            }
        }
        if saw_frames {
            panic!("stream carried frames but no End");
        }
        Sealed::Nothing
    }

    fn state_of(
        req: Request,
    ) -> (
        ExchangeState,
        tokio::sync::mpsc::Receiver<crate::types::Frame>,
    ) {
        // room for a full event trio with no reader (seal must not park)
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let job = Box::new(Job {
            ctx: Context::new(req, tx, /*superglobals=*/ false),
        });
        let Ok(st) = ExchangeState::new(job) else {
            unreachable!("empty cursor body always reads")
        };
        (st, rx)
    }

    fn state() -> (
        ExchangeState,
        tokio::sync::mpsc::Receiver<crate::types::Frame>,
    ) {
        state_of(base_req())
    }

    /// The buffer cap must seal (truncated) rather than merely error: an
    /// unsealed overflow leaves the unit in Handling and wedges every later
    /// receive() on the single-flight check for the life of the worker. The
    /// oversized `len` is checked before the byte slice is formed, so no giant
    /// buffer is needed.
    #[test]
    fn overflow_seals_the_unit_truncated() {
        let (mut st, mut rx) = state();
        let v = unsafe { write_body_core(&mut st, c"x".as_ptr(), MAX_BUFFERED_BODY + 1, false) };
        assert_eq!(v, Verb::Overflow);

        let Sealed::Truncated { status, body } = recv_sealed(&mut rx) else {
            panic!("overflow must seal a truncated stream");
        };
        assert_eq!(status, Some(200));
        assert!(body.is_empty(), "the overflowing chunk is never sent");

        // The unit is concluded: later verbs see Finalized, not a wedge.
        let v = unsafe { write_body_core(&mut st, c"y".as_ptr(), 1, true) };
        assert_eq!(v, Verb::Finalized);
        let job: *const c_void = (&raw const st).cast();
        assert!(unsafe { rapira_rs_exchange_is_finalized(job) });
    }

    /// A 304 head drops accepted body chunks at seal, like 204 and HEAD.
    #[test]
    fn seal_drops_the_body_for_304() {
        let (mut st, mut rx) = state();
        assert_eq!(
            unsafe { write_head_core(&mut st, 304, Vec::new()) },
            Verb::Ok
        );
        let v = unsafe { write_body_core(&mut st, c"gone".as_ptr(), 4, true) };
        assert_eq!(v, Verb::Ok);
        let Sealed::Complete { status, body } = recv_sealed(&mut rx) else {
            panic!("must seal cleanly");
        };
        assert_eq!(status, 304);
        assert!(body.is_empty(), "304 carries no body");
    }

    /// Contract: an empty chunk without eos does nothing - no head commits.
    #[test]
    fn empty_non_eos_chunk_commits_nothing() {
        let (mut st, mut rx) = state();
        let v = unsafe { write_body_core(&mut st, c"".as_ptr(), 0, false) };
        assert_eq!(v, Verb::Ok);
        assert_eq!(
            unsafe { write_head_core(&mut st, 404, Vec::new()) },
            Verb::Ok,
            "the head slot must still be open"
        );
        let v = unsafe { write_body_core(&mut st, c"x".as_ptr(), 1, true) };
        assert_eq!(v, Verb::Ok);
        let Sealed::Complete { status, .. } = recv_sealed(&mut rx) else {
            panic!("must seal cleanly");
        };
        assert_eq!(status, 404);
    }

    /// The wire validators mirror the classic path's byte sets exactly.
    #[test]
    fn wire_validators_match_the_classic_byte_sets() {
        assert!(wire_token(b"x-trace"));
        assert!(!wire_token(b""));
        assert!(!wire_token(b"bad name"));
        assert!(!wire_token(b"x:y"));
        assert!(wire_value(b"a\tb \xff"));
        assert!(!wire_value(b"a\x01b"));
        assert!(!wire_value(b"a\x7fb"));
        assert!(!wire_value(b"split\r\nx: y"));
        assert!(!wire_value(b"nul\0"));
    }

    /// Construction normalizes the contract protocol spelling and treats an
    /// empty unix path as the unnamed endpoint.
    #[test]
    fn construction_normalizes_protocol_and_empty_unix_path() {
        let mut req = base_req();
        req.protocol = "HTTP/3.0".into();
        req.remote = Addr::Unix(Some(PathBuf::new()));
        let (st, _rx) = state_of(req);
        assert_eq!(st.protocol_php, "HTTP/3");
        assert!(matches!(st.remote, AddrOwned::Unix(None)));
    }

    /// A one-shot body write carries its computed length on the Head frame; a
    /// streamed first write leaves the framing to the front.
    #[test]
    fn head_frame_length_follows_the_write_shape() {
        use crate::types::Frame;
        let (mut st, mut rx) = state();
        let v = unsafe { write_body_core(&mut st, c"abc".as_ptr(), 3, true) };
        assert_eq!(v, Verb::Ok);
        let Ok(Frame::Head { content_length, .. }) = rx.try_recv() else {
            panic!("head first");
        };
        assert_eq!(content_length, Some(3));

        let (mut st, mut rx) = state();
        let v = unsafe { write_body_core(&mut st, c"abc".as_ptr(), 3, false) };
        assert_eq!(v, Verb::Ok);
        let Ok(Frame::Head { content_length, .. }) = rx.try_recv() else {
            panic!("head first");
        };
        assert_eq!(content_length, None, "streaming: the front frames");
    }

    /// The CLEE prefix rule: the fitting bytes go out, the response completes
    /// per its declaration (not truncated), and later writes see Finalized.
    #[test]
    fn content_length_exceeded_sends_the_prefix_and_seals() {
        use crate::types::Frame;
        let (mut st, mut rx) = state();
        let v = unsafe {
            write_head_core(&mut st, 200, vec![("content-length".into(), b"5".to_vec())])
        };
        assert_eq!(v, Verb::Ok);
        let v = unsafe { write_body_core(&mut st, c"0123456789".as_ptr(), 10, true) };
        assert_eq!(v, Verb::ContentLengthExceeded);

        let Ok(Frame::Head { content_length, .. }) = rx.try_recv() else {
            panic!("head first");
        };
        assert_eq!(content_length, Some(5), "the declared length is honoured");
        let Ok(Frame::Chunk(b)) = rx.try_recv() else {
            panic!("the fitting prefix must be sent");
        };
        assert_eq!(&b[..], b"01234");
        let Ok(Frame::End { truncated, .. }) = rx.try_recv() else {
            panic!("sealed");
        };
        assert!(!truncated, "complete per its declaration");

        let v = unsafe { write_body_core(&mut st, c"x".as_ptr(), 1, true) };
        assert_eq!(v, Verb::Finalized, "nothing written after it");
    }

    /// A repeated content-length in the head table is a `\ValueError`.
    #[test]
    fn repeated_content_length_is_a_bad_field() {
        let (mut st, _rx) = state();
        let v = unsafe {
            write_head_core(
                &mut st,
                200,
                vec![
                    ("content-length".into(), b"5".to_vec()),
                    ("Content-Length".into(), b"7".to_vec()),
                ],
            )
        };
        assert!(matches!(v, Verb::BadField(_)));
        assert_eq!(st.stage, Stage::Open, "a rejected head commits nothing");
    }

    /// An interim head emits at once, minus framing fields, and leaves the
    /// final-head slot open.
    #[test]
    fn interim_head_emits_without_framing_fields() {
        use crate::types::Frame;
        let (mut st, mut rx) = state();
        let v = unsafe {
            write_head_core(
                &mut st,
                103,
                vec![
                    ("link".into(), b"</a.css>; rel=preload".to_vec()),
                    ("content-length".into(), b"5".to_vec()),
                    ("connection".into(), b"close".to_vec()),
                ],
            )
        };
        assert_eq!(v, Verb::Interim);
        let Ok(Frame::Interim(head)) = rx.try_recv() else {
            panic!("interim head must be on the stream");
        };
        assert_eq!(head.status, 103);
        assert_eq!(
            head.headers.len(),
            1,
            "framing fields stripped: {:?}",
            head.headers
        );
        assert_eq!(head.headers[0].0, "link");
        let v = unsafe { write_head_core(&mut st, 200, Vec::new()) };
        assert_eq!(v, Verb::Ok, "the final-head slot stays open");
    }

    /// A gone client discards the unit exactly once; the latch is sticky and
    /// the unit reports finalized + cancelled.
    #[test]
    fn gone_client_discards_once_and_stays_discarded() {
        let (mut st, rx) = state();
        drop(rx);
        let v = unsafe { write_body_core(&mut st, c"x".as_ptr(), 1, false) };
        assert_eq!(v, Verb::Discarded);
        let v = unsafe { write_body_core(&mut st, c"y".as_ptr(), 1, true) };
        assert_eq!(v, Verb::Discarded, "sticky across repeat writes");

        let job: *const c_void = (&raw const st).cast();
        assert!(unsafe { rapira_rs_exchange_is_finalized(job) });
        assert!(unsafe { rapira_rs_exchange_is_cancelled(job) });
    }

    /// `flush()` commits and emits the implicit 200 once; a repeat flush puts
    /// nothing new on the stream.
    #[test]
    fn flush_emits_the_implicit_head_once() {
        use crate::types::Frame;
        let (mut st, mut rx) = state();
        let job: *mut c_void = (&raw mut st).cast();
        assert!(unsafe { rapira_rs_exchange_flush(job) });
        let Ok(Frame::Head {
            head,
            content_length,
            ..
        }) = rx.try_recv()
        else {
            panic!("flush must emit the head");
        };
        assert_eq!(head.status, 200);
        assert!(head.headers.is_empty(), "implicit 200 has no fields");
        assert_eq!(content_length, None, "flush costs the computed length");
        assert!(unsafe { rapira_rs_exchange_flush(job) });
        assert!(rx.try_recv().is_err(), "a repeat flush is a no-op");
    }

    /// A committed 101 is bodiless: chunks are accepted and dropped.
    #[test]
    fn a_101_head_drops_body_chunks() {
        use crate::types::Frame;
        let (mut st, mut rx) = state();
        assert_eq!(
            unsafe { write_head_core(&mut st, 101, Vec::new()) },
            Verb::Ok
        );
        let v = unsafe { write_body_core(&mut st, c"upgrade".as_ptr(), 7, true) };
        assert_eq!(v, Verb::Ok);
        let Ok(Frame::Head { bodiless, .. }) = rx.try_recv() else {
            panic!("head first");
        };
        assert!(bodiless);
        assert!(
            matches!(rx.try_recv(), Ok(Frame::End { .. })),
            "no chunk frames for a 1xx response"
        );
    }

    /// sendFile validation, one test fn: the root is process-global state.
    #[test]
    fn send_file_validation_table() {
        use crate::types::Frame;
        let dir = std::env::temp_dir();
        set_sendfile_root(dir.clone());
        let path = dir.join(format!("rapira-sf-{}", std::process::id()));
        std::fs::write(&path, b"abcdefghijklmnopqrstuvwxyz").unwrap();
        let pb = path_bytes(&path);
        let link_out = dir.join(format!("rapira-sf-out-{}", std::process::id()));
        std::fs::remove_file(&link_out).ok();
        std::os::unix::fs::symlink("/etc/hosts", &link_out).unwrap();

        let (mut st, _rx) = state();
        for (name, path, offset, length) in [
            ("missing", b"/definitely/not/here".to_vec(), 0, None),
            ("directory", path_bytes(&dir), 0, None),
            ("offset past end", pb.clone(), 27, None),
            ("slice past end", pb.clone(), 20, Some(10)),
            ("outside the root", b"/etc/hosts".to_vec(), 0, None),
            ("escaping symlink", path_bytes(&link_out), 0, None),
        ] {
            let v = unsafe { send_file_core(&mut st, &path, offset, length, true) };
            assert!(matches!(v, Verb::FileNotSendable(_)), "{name}");
        }
        // raised before anything is written: the unit stays open
        assert_eq!(st.stage, Stage::Open);

        let (mut st, mut rx) = state();
        let v = unsafe { send_file_core(&mut st, &pb, 2, Some(3), true) };
        assert_eq!(v, Verb::Ok);
        let Ok(Frame::Head { content_length, .. }) = rx.try_recv() else {
            panic!("head first");
        };
        assert_eq!(
            content_length,
            Some(3),
            "the slice length is known up front"
        );
        let Ok(Frame::File { offset, len, .. }) = rx.try_recv() else {
            panic!("the file rides its own frame");
        };
        assert_eq!((offset, len), (2, 3));
        assert!(matches!(
            rx.try_recv(),
            Ok(Frame::End {
                truncated: false,
                ..
            })
        ));
        assert_eq!(st.stage, Stage::Finalized);

        // a symlink whose target stays inside the root is allowed
        let link_in = dir.join(format!("rapira-sf-in-{}", std::process::id()));
        std::fs::remove_file(&link_in).ok();
        std::os::unix::fs::symlink(&path, &link_in).unwrap();
        let (mut st, mut rx) = state();
        let v = unsafe { send_file_core(&mut st, &path_bytes(&link_in), 0, None, true) };
        assert_eq!(v, Verb::Ok, "intra-root symlinks stay sendable");
        assert!(matches!(rx.try_recv(), Ok(Frame::Head { .. })));

        std::fs::remove_file(&link_in).ok();
        std::fs::remove_file(&link_out).ok();
        std::fs::remove_file(&path).ok();
    }

    /// The fd readback used for the post-open containment re-check must agree
    /// with the canonical path on both supported platforms.
    #[test]
    fn fd_path_reads_back_the_real_path() {
        let p = std::env::temp_dir().join(format!("rapira-fdp-{}", std::process::id()));
        std::fs::write(&p, b"x").unwrap();
        let f = std::fs::File::open(&p).unwrap();
        let held = fd_path(&f).expect("fd readback works on this platform");
        assert_eq!(held, std::fs::canonicalize(&p).unwrap());
        std::fs::remove_file(&p).ok();
    }

    /// Trailers end the response through the End frame; repeat calls land on
    /// Finalized, and a headless call is HeadNotWritten.
    #[test]
    fn trailers_finalize_with_a_committed_head() {
        use crate::types::Frame;
        let (mut st, mut rx) = state();
        let v = unsafe { write_trailers_core(&mut st, vec![("x".into(), b"y".to_vec())]) };
        assert_eq!(v, Verb::HeadNotWritten, "nothing here commits a head");

        assert_eq!(
            unsafe { write_head_core(&mut st, 200, Vec::new()) },
            Verb::Ok
        );
        let v = unsafe { write_trailers_core(&mut st, vec![("x".into(), b"y".to_vec())]) };
        assert_eq!(v, Verb::Ok);
        let Ok(Frame::Head { content_length, .. }) = rx.try_recv() else {
            panic!("head first");
        };
        assert_eq!(
            content_length,
            Some(0),
            "trailers-only keeps length framing"
        );
        let Ok(Frame::End {
            trailers,
            truncated,
        }) = rx.try_recv()
        else {
            panic!("the trailers ride the End frame");
        };
        assert!(!truncated);
        assert_eq!(trailers, vec![("x".to_string(), b"y".to_vec())]);

        let v = unsafe { write_trailers_core(&mut st, Vec::new()) };
        assert_eq!(v, Verb::Finalized);
    }

    /// The forbidden set covers every RFC 9110 §6.5.1 category; unknown
    /// extension fields pass.
    #[test]
    fn trailer_denylist_matches_the_categories() {
        for name in [
            "Content-Length",
            "connection",
            "host",
            "authorization",
            "cache-control",
            "date",
            "content-type",
        ] {
            assert!(forbidden_trailer(name), "{name}");
        }
        assert!(!forbidden_trailer("x-checksum"));
        assert!(!forbidden_trailer("server-timing"));
    }

    /// Sealing unlinks the spool files (the contract's "gone when the exchange
    /// finalizes"); the Drop net stays idempotent afterwards.
    #[test]
    fn seal_unlinks_the_spool_files() {
        let (mut st, mut _rx) = state();
        let dir = std::env::temp_dir();
        let path = dir.join(format!("rapira-test-spool-{}", std::process::id()));
        std::fs::write(&path, b"payload").unwrap();
        st.body = BodyState::Multipart {
            fields: Vec::new(),
            files: vec![FilePart {
                upload: crate::types::UploadedFile {
                    name: b"f".to_vec(),
                    client_filename: b"a.bin".to_vec(),
                    client_media_type: None,
                    headers: Vec::new(),
                    file: crate::types::SpooledFile { path: path.clone() },
                    size: 7,
                },
                path: path_bytes(&path),
                headers: Grouped::new(&[]),
            }],
        };
        assert!(path.exists());
        unsafe { seal(&mut st, false, Vec::new()) };
        assert!(!path.exists(), "seal must unlink the spooled file");
    }
}
