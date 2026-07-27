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
