use crate::harness::*;
use std::time::{Duration, Instant};

#[test]
fn static_pool_forks_n_workers() {
    let srv = spawn_with_config("echo-worker.php", 3, "");
    wait_workers(&srv, Duration::from_secs(20), "3 static workers", |p| {
        p.len() == 3
    });
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(
        code,
        200,
        "pool should serve once up\n{}",
        diagnostics(&srv)
    );
}

#[test]
fn http_round_trip() {
    let srv = spawn_with_config("echo-worker.php", 1, "");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
    for _ in 0..2 {
        let (code, body) =
            http_get(srv.addr, "/?from=e2e", Duration::from_secs(10)).expect("GET /?from=e2e");
        assert_eq!(code, 200, "\n{}", diagnostics(&srv));
        assert!(
            body.starts_with(b"ok:"),
            "body should start with ok:, got {:?}",
            String::from_utf8_lossy(&body)
        );
    }
}

#[test]
fn killed_worker_respawns() {
    let srv = spawn_with_config("echo-worker.php", 2, "");
    let pids0 = wait_workers(&srv, Duration::from_secs(20), "2 workers", |p| p.len() == 2);
    signal(pids0[0], libc::SIGKILL);
    wait_workers(
        &srv,
        Duration::from_secs(20),
        "respawn to 2 with a fresh pid",
        |p| p.len() == 2 && p.iter().any(|x| !pids0.contains(x)),
    );
    for _ in 0..5 {
        let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
        assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    }
}

// After the master exits its workers reparent away, so `worker_pids` can no
// longer see them — poll the captured pids directly until every one is gone.
fn wait_pids_gone(pids: &[u32], timeout: Duration, srv: &Server) {
    let end = Instant::now() + timeout;
    loop {
        // SAFETY: kill(pid, 0) only probes existence; ESRCH means gone.
        let gone = pids
            .iter()
            .all(|&p| unsafe { libc::kill(p as libc::pid_t, 0) } == -1);
        if gone {
            return;
        }
        assert!(
            Instant::now() < end,
            "workers survived the master: {pids:?}\n{}",
            diagnostics(srv)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

// Stop budget: past process_control_timeout (30s) the master escalates a stuck
// worker QUIT → TERM → KILL and still exits 0, so the wait must outlast it.
const STOP_BUDGET: Duration = Duration::from_secs(45);

#[test]
fn sigquit_master_graceful() {
    let mut srv = spawn_with_config("echo-worker.php", 2, "");
    let pids = wait_workers(&srv, Duration::from_secs(20), "2 workers", |p| p.len() == 2);
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    signal(srv.pid(), libc::SIGQUIT);
    let status = srv.wait_exit(STOP_BUDGET);
    assert_exit_code(status, MASTER_EXIT_OK, &srv);
    wait_pids_gone(&pids, Duration::from_secs(10), &srv);
}

#[test]
fn sigterm_master_stops() {
    let mut srv = spawn_with_config("echo-worker.php", 2, "");
    let pids = wait_workers(&srv, Duration::from_secs(20), "2 workers", |p| p.len() == 2);
    signal(srv.pid(), libc::SIGTERM);
    let status = srv.wait_exit(STOP_BUDGET);
    assert_exit_code(status, MASTER_EXIT_OK, &srv);
    wait_pids_gone(&pids, Duration::from_secs(10), &srv);
}

#[test]
fn max_requests_recycles() {
    let srv = spawn_with_config("echo-worker.php", 1, "[pm]\nmax_requests = 5\n");
    let pids0 = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
    let pid0 = pids0[0];
    // The backlog covers the swap gap: the master never closes the listen fd, so
    // every request is served across recycles.
    for _ in 0..40 {
        let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
        assert_eq!(
            code,
            200,
            "request lost across a recycle\n{}",
            diagnostics(&srv)
        );
    }
    wait_workers(
        &srv,
        Duration::from_secs(30),
        "worker recycled to a new pid",
        |p| p.len() == 1 && p[0] != pid0,
    );
}

#[test]
fn request_timeout_kills_and_replaces_worker() {
    let srv = spawn_with_config(
        "hang-worker.php",
        1,
        "[pm]\nrequest_terminate_timeout_secs = 2\n",
    );
    let pids0 = wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
    // Sanity: the worker serves before it is asked to hang.
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    // The hanging request pins the worker ACTIVE past the 2s limit; the
    // watchdog TERMs it, so the client sees a reset/EOF, never a response.
    let hung = http_get(srv.addr, "/?hang=1", Duration::from_secs(15));
    assert!(
        hung.is_err(),
        "hung request must die with the worker, got {hung:?}\n{}",
        diagnostics(&srv)
    );
    // The kill is a TimeoutKill: replaced immediately, no backoff.
    wait_workers(
        &srv,
        Duration::from_secs(20),
        "worker replaced after timeout kill",
        |p| p.len() == 1 && p[0] != pids0[0],
    );
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET / after kill");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
}

#[test]
fn master_failboot_exits_70() {
    let mut srv = spawn_with_config("fatal-worker.php", 1, "");
    let addr = srv.addr;
    let end = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(st) = srv.try_status() {
            break Some(st);
        }
        if Instant::now() >= end {
            panic!("master never exited\n{}", diagnostics(&srv));
        }
        // Load-bearing: each request pumps one boot retry (strikes are demand-driven).
        // 503s / connection errors are expected until the worker strikes out and the
        // master exits.
        let _ = http_get(addr, "/", Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_exit_code(status, MASTER_EXIT_FAILBOOT, &srv);
}

/// A field php-src let through but no front can represent must cost only that field.
/// Reachable only over a real socket: the 500 is synthesized inside pingora, below the
/// in-process harness.
#[test]
fn unrepresentable_header_still_serves_the_response() {
    let srv = spawn_with_config("bad-header-worker.php", 1, "");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
    let (code, body) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(code, 201, "\n{}", diagnostics(&srv));
    assert_eq!(body, b"body", "\n{}", diagnostics(&srv));
}

/// The multipart boundary is opaque octets and must reach php-src byte for byte: decode
/// it lossily and rfc1867 searches for a boundary the body never contains, so the upload
/// silently vanishes. This is the only level that covers the rapira_runtime mapping.
#[test]
fn non_utf8_multipart_boundary_uploads() {
    let srv = spawn_with_config("upload-worker.php", 1, "");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
    let boundary: &[u8] = b"RAP\xff\xfeIRA";
    let mut body = Vec::new();
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary);
    body.extend_from_slice(b"\r\nContent-Disposition: form-data; name=\"file\"; filename=\"foo.txt\"\r\nContent-Type: text/plain\r\n\r\nbar\r\n--");
    body.extend_from_slice(boundary);
    body.extend_from_slice(b"--\r\n");
    let mut ctype = b"multipart/form-data; boundary=".to_vec();
    ctype.extend_from_slice(boundary);

    let (code, out) =
        http_post(srv.addr, "/", &ctype, &body, Duration::from_secs(10)).expect("POST /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    let out = String::from_utf8_lossy(&out);
    assert!(
        out.starts_with("foo.txt|0|bar|"),
        "upload must parse (got {out:?})\n{}",
        diagnostics(&srv)
    );
}

/// A field sent more than once reaches PHP as one value: a comma list, and `"; "` for
/// Cookie. Only observable over a real socket — the in-process harness builds a request
/// whose fields are already combined.
#[test]
fn repeated_request_fields_reach_php_combined() {
    let srv = spawn_with_config("repeated-headers-worker.php", 1, "");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
    let (code, body) = http_get_with_headers(
        srv.addr,
        "/",
        &[
            ("Cookie", "a=1"),
            ("Cookie", "b=2"),
            ("X-Forwarded-For", "203.0.113.7"),
            ("X-Forwarded-For", "10.0.0.1"),
        ],
        Duration::from_secs(10),
    )
    .expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert_eq!(
        String::from_utf8_lossy(&body),
        "1,2\na=1; b=2\n203.0.113.7, 10.0.0.1\n",
        "\n{}",
        diagnostics(&srv)
    );
}

/// A wire name carrying `_` or `.` maps onto the CGI variable a `-` name owns. The `.`
/// half of that only closes end to end, because PHP is what rewrites `.` to `_` when it
/// registers the variable — the front never produces the colliding name itself.
#[test]
fn alias_names_never_reach_a_cgi_variable() {
    let srv = spawn_with_config("repeated-headers-worker.php", 1, "");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
    let (code, body) = http_get_with_headers(
        srv.addr,
        "/",
        &[
            ("X_Forwarded_For", "1.2.3.4"),
            ("X.Forwarded.For", "5.6.7.8"),
        ],
        Duration::from_secs(10),
    )
    .expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert_eq!(
        String::from_utf8_lossy(&body),
        "-,-\n-\n-\n",
        "no alias may reach HTTP_X_FORWARDED_FOR\n{}",
        diagnostics(&srv)
    );
}

/// `reject` turns the module's HTTPStatus(400) into a real 400 on the wire — that
/// translation happens in pingora's fail_to_proxy, so only an e2e run proves it.
#[test]
fn reject_policy_answers_400_for_an_alias_name() {
    let srv = spawn_with_http_extra(
        "repeated-headers-worker.php",
        1,
        "unsafe_field_names = \"reject\"\n",
    );
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);

    let (code, _) = http_get_with_headers(
        srv.addr,
        "/",
        &[("X_Forwarded_For", "1.2.3.4")],
        Duration::from_secs(10),
    )
    .expect("GET / with an alias name");
    assert_eq!(code, 400, "\n{}", diagnostics(&srv));

    // A request with no unsafe name is untouched by the policy.
    let (code, _) = http_get_with_headers(
        srv.addr,
        "/",
        &[("X-Forwarded-For", "203.0.113.7")],
        Duration::from_secs(10),
    )
    .expect("GET / with a safe name");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
}

/// `header("Status: 404")` must become the response code, not a literal field on a 200.
/// php-src's sapi_header_op does not special-case it, so the SAPI is what has to consume
/// it — the CGI SAPI does (cgi_main.c) and nginx additionally hides it from the client.
#[test]
fn status_field_sets_the_code_and_never_reaches_the_client() {
    let srv = spawn_with_config("status-header-worker.php", 1, "");
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
    let raw = http_get_raw(srv.addr, "/", &[], Duration::from_secs(10)).expect("GET /");
    let text = String::from_utf8_lossy(&raw).to_ascii_lowercase();

    assert!(
        text.starts_with("http/1.1 404"),
        "Status: must set the response code (got {:?})\n{}",
        text.lines().next().unwrap_or(""),
        diagnostics(&srv)
    );
    assert!(
        !text.contains("\r\nstatus:"),
        "Status: must not reach the client\n{}",
        diagnostics(&srv)
    );
    assert!(
        text.contains("x-keep: kept"),
        "other fields must survive\n{}",
        diagnostics(&srv)
    );
}
