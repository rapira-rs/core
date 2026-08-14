//! The Rust half of `Rapira\Internal\Http\Exchange`: owns the `Job` while PHP
//! holds the unit, builds the `Rapira\Http\Request` graph, marshals the
//! response verbs in, and seals through the single-Frame `Context::finish`
//! path. The graph builders follow the zend.rs frame rules.

use std::{
    cell::Cell,
    ffi::{CString, c_char, c_int, c_void},
    io::Read,
    path::Path,
    time::Duration,
};

use crate::{
    HashPosition, HashTable, IS_ARRAY, IS_STRING, RAPIRA_MODE_DISPATCHER, add_assoc_zval_ex,
    add_next_index_object,
    callbacks::{MAX_BUFFERED_BODY, guard, is_field_value_byte, is_tchar},
    object_init_ex, rapira_array_init, rapira_ce_already_finalized_error,
    rapira_ce_closed_exception, rapira_ce_http_form_field,
    rapira_ce_http_head_already_written_error, rapira_ce_http_multipart, rapira_ce_http_request,
    rapira_ce_http_tls, rapira_ce_http_uploaded_file, rapira_ce_inet_address,
    rapira_ce_internal_http_dispatcher, rapira_ce_internal_http_dispatcher_info,
    rapira_ce_internal_http_exchange, rapira_ce_not_in_worker_mode_error,
    rapira_ce_timeout_exception, rapira_ce_unix_address, rapira_dispatcher_info_obj,
    rapira_exchange_obj, rapira_receive_timed, rapira_receive_untimed,
    scoreboard::{Event, sb_update},
    start::{Pulled, pending_depth, pull_job_try, pull_job_wait},
    types::{Addr, Body, FormField, Job, ResponseHead, StreamState, TlsView, UploadedFile},
    zend, zend_class_entry, zend_hash_get_current_data_ex, zend_hash_get_current_key_ex,
    zend_hash_internal_pointer_reset_ex, zend_hash_move_forward_ex, zend_object, zend_string, zval,
    zval_add_ref, zval_ptr_dtor,
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

pub(crate) fn served_any() -> bool {
    CYCLE.get().served
}

pub(crate) fn received_any() -> bool {
    CYCLE.get().received
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
    // Frame sender closes and the client observes the failure — the same
    // file-gone-first order seal() guarantees.
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
        })
    }

    /// The one head-commit site: Stage, the recorded head, and the stream
    /// marker are three fields carrying one fact.
    fn commit_head(&mut self, head: ResponseHead) {
        self.job.ctx.head = Some(head);
        self.job.ctx.stream = StreamState::HeadSent;
        self.stage = Stage::HeadCommitted;
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
/// loops — property writes with a pending exception are what the checkpoints
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
/// exchange. Contract with the C shell: false means a throw is pending — a
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
                rapira_ce_not_in_worker_mode_error,
                c"nothing dispatches work to this process outside worker mode",
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

/// Verb outcomes; only the non-Ok arms surface to PHP, as throws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Ok,
    /// Advisory 1xx head, dropped by design; nothing thrown.
    Interim,
    Finalized,
    HeadWritten,
    Overflow,
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
            Verb::Overflow => zend::throw_error(c"response exceeds the host buffer cap"),
        }
    }
}

/// RFC 9110 §5.6.2 tchar. A non-token name would pass a weaker check and then
/// be dropped silently downstream instead of raising the promised ValueError.
/// https://www.rfc-editor.org/rfc/rfc9110#section-5.6.2
fn wire_token(name: &[u8]) -> bool {
    !name.is_empty() && name.iter().all(|&b| is_tchar(b))
}

/// Field values per RFC 9110 §5.5 — the same byte set the classic path
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
            let entry = zend_hash_get_current_data_ex(ht, &pos);
            if entry.is_null() {
                break;
            }
            let mut str_key: *mut zend_string = std::ptr::null_mut();
            let mut num_key = 0;
            let kt = zend_hash_get_current_key_ex(ht, &mut str_key, &mut num_key, &pos);
            if kt != crate::zend_hash_key_type_HASH_KEY_IS_STRING || str_key.is_null() {
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
                let item = zend_hash_get_current_data_ex(inner, &ipos);
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

fn write_head_core(st: &mut ExchangeState, status: u16, headers: Vec<(String, Vec<u8>)>) -> Verb {
    // Finalized implies a committed head, so one gate covers both.
    if st.stage != Stage::Open {
        return Verb::HeadWritten;
    }
    // 101 ends the HTTP conversation and counts as a final head; the other
    // 1xx are interim — advisory, and this host emits none yet.
    if status != 101 && (100..200).contains(&status) {
        return Verb::Interim;
    }
    // Direct set, not Context::commit_head — that path implements the CGI
    // `Status:` override, which must not exist on the Exchange surface.
    st.commit_head(ResponseHead { status, headers });
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
/// before the slice is formed).
unsafe fn write_body_core(st: &mut ExchangeState, p: *const c_char, len: usize, eos: bool) -> Verb {
    if st.stage == Stage::Finalized {
        return Verb::Finalized;
    }
    // Contract: an empty chunk without eos does nothing — it is how a
    // chunked body terminates, never a head commit.
    if len == 0 && !eos {
        return Verb::Ok;
    }
    // The contract commits an implicit 200 before the first body chunk,
    // even a buffered eos:false one — a later writeHead must see
    // HeadAlreadyWritten, not retroactively restamp the status.
    if st.stage == Stage::Open {
        st.commit_head(ResponseHead {
            status: 200,
            headers: Vec::new(),
        });
    }
    let ctx = &mut st.job.ctx;
    if len > 0 {
        if ctx.body.len() + len > MAX_BUFFERED_BODY {
            tracing::error!(
                target: "rapira",
                "response body exceeds the host buffer cap ({} + {len} > {MAX_BUFFERED_BODY} bytes); sealing truncated",
                ctx.body.len()
            );
            // Seal as truncated so the unit concludes (upstream reports the
            // failure) instead of wedging the worker on the next receive().
            seal(st, /*truncated=*/ true);
            return Verb::Overflow;
        }
        let bytes = unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) };
        ctx.body.extend_from_slice(bytes);
    }
    if eos {
        seal(st, /*truncated=*/ false);
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
    ctx.finish(truncated);
}

/// # Safety
/// `job` from receive.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rapira_rs_exchange_is_finalized(job: *const c_void) -> bool {
    // false on a caught panic: claiming "ended" on an open unit would steer a
    // conforming worker into dropping it.
    guard(false, || unsafe {
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

    fn state_of(
        req: Request,
    ) -> (
        ExchangeState,
        tokio::sync::mpsc::Receiver<crate::types::Frame>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
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

        let frame = rx.blocking_recv().expect("overflow must seal a frame");
        assert!(frame.truncated, "the sealed frame reports truncation");
        assert_eq!(frame.head.map(|h| h.status), Some(200));

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
        assert_eq!(write_head_core(&mut st, 304, Vec::new()), Verb::Ok);
        let v = unsafe { write_body_core(&mut st, c"gone".as_ptr(), 4, true) };
        assert_eq!(v, Verb::Ok);
        let frame = rx.blocking_recv().expect("sealed");
        assert_eq!(frame.head.map(|h| h.status), Some(304));
        assert!(frame.body.is_empty(), "304 carries no body");
    }

    /// Contract: an empty chunk without eos does nothing — no head commits.
    #[test]
    fn empty_non_eos_chunk_commits_nothing() {
        let (mut st, mut rx) = state();
        let v = unsafe { write_body_core(&mut st, c"".as_ptr(), 0, false) };
        assert_eq!(v, Verb::Ok);
        assert_eq!(
            write_head_core(&mut st, 404, Vec::new()),
            Verb::Ok,
            "the head slot must still be open"
        );
        let v = unsafe { write_body_core(&mut st, c"x".as_ptr(), 1, true) };
        assert_eq!(v, Verb::Ok);
        let frame = rx.blocking_recv().expect("sealed");
        assert_eq!(frame.head.map(|h| h.status), Some(404));
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
        seal(&mut st, false);
        assert!(!path.exists(), "seal must unlink the spooled file");
    }
}
