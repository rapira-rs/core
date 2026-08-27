//! CI-only APM agent suites. The Linux CI legs place newrelic.so and ddtrace.so into
//! the PHP extension dir and do not enable them globally. Each suite loads its agent
//! through its own php.ini and skips when the object is absent.
//!
//! Not tested here: newrelic's fatal-signal handler. It runs per worker, covers
//! SIGSEGV/SIGBUS/SIGFPE/SIGILL/SIGABRT, and re-raises with the default disposition.
//! rapira registers none of those signals (crates/master/src/signals.rs), and crash
//! classification reads the wait status, so the handler cannot change what the
//! master observes.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::harness::{
    Server, diagnostics, http_get, php_extension, php_extension_dir, spawn_with_phprc_and_config,
    wait_log_contains, wait_workers, worker_pids,
};

const REQ: Duration = Duration::from_secs(10);
const BOOT_POOL: Duration = Duration::from_secs(20);

/// A pool with the New Relic agent loaded for one test. The 40-char license is a
/// dummy: local validation checks only the length. `dont_launch = 3` stops every
/// daemon fork. The debug log in the scratch dir feeds the late-init assertion.
fn newrelic_srv(processes: usize) -> Option<Server> {
    php_extension("newrelic.so")?;
    let ini = format!(
        "extension_dir = \"{}\"\n\
         extension = newrelic.so\n\
         newrelic.enabled = 1\n\
         newrelic.appname = \"rapira-e2e\"\n\
         newrelic.license = \"0123456789abcdef0123456789abcdef01234567\"\n\
         newrelic.daemon.dont_launch = 3\n\
         newrelic.loglevel = debug\n\
         newrelic.logfile = \"newrelic-agent.log\"\n\
         newrelic.daemon.logfile = \"newrelic-daemon.log\"\n\
         display_errors = On\n\
         error_reporting = E_ALL\n\
         max_execution_time = 0\n\
         session.gc_probability = 0\n",
        php_extension_dir()?.display()
    );
    Some(spawn_with_phprc_and_config(
        "apm/newrelic-worker.php",
        processes,
        &ini,
        "mode = \"worker\"\n",
    ))
}

/// One request whose body decides skip-vs-run for the whole test.
fn probe(srv: &Server) -> Option<String> {
    let (code, body) = http_get(srv.addr, "/", REQ).expect("GET /");
    let body = String::from_utf8_lossy(&body).into_owned();
    if body == "skip" {
        return None;
    }
    assert_eq!(code, 200, "probe failed: {body:?}\n{}", diagnostics(srv));
    Some(body)
}

fn nr_pid(body: &str, srv: &Server) -> u32 {
    let rest = body
        .strip_prefix("nr:ok:")
        .unwrap_or_else(|| panic!("unexpected body {body:?}\n{}", diagnostics(srv)));
    let (version, pid) = rest
        .rsplit_once(':')
        .unwrap_or_else(|| panic!("unexpected body {body:?}"));
    assert!(!version.is_empty(), "phpversion('newrelic') must be set");
    pid.parse()
        .unwrap_or_else(|_| panic!("bad pid in {body:?}"))
}

/// Every newrelic MINIT branch returns SUCCESS, and all newrelic_* functions register
/// unconditionally. With no daemon and a dummy license the agent must load, expose
/// its API, and leave the pool serving.
#[test]
fn newrelic_loads_and_the_pool_keeps_serving() {
    let Some(srv) = newrelic_srv(2) else { return };
    let before = wait_workers(&srv, BOOT_POOL, "2 workers", |p| p.len() == 2);
    let Some(first) = probe(&srv) else { return };
    nr_pid(&first, &srv);
    for _ in 0..20 {
        let (code, body) = http_get(srv.addr, "/", REQ).expect("GET /");
        let body = String::from_utf8_lossy(&body);
        assert_eq!(code, 200, "got {body:?}\n{}", diagnostics(&srv));
        let pid = nr_pid(&body, &srv);
        assert!(
            before.contains(&pid),
            "answering pid {pid} must be one of the workers {before:?}"
        );
    }
    assert_eq!(
        worker_pids(srv.pid()),
        before,
        "no worker was replaced under the agent\n{}",
        diagnostics(&srv)
    );
}

/// The agent finishes its per-process setup at the first RINIT (late init). Under a
/// pre-fork pool it must run once in every worker. The master contributes only the
/// MINIT lines. Every agent log line carries "(<pid> <tid>)" in its preamble
/// (axiom/util_logging.c). The assertion uses the pid, not the message text, so a
/// changed agent message does not break it.
#[test]
fn newrelic_late_init_runs_in_every_worker() {
    let Some(srv) = newrelic_srv(2) else { return };
    wait_workers(&srv, BOOT_POOL, "2 workers", |p| p.len() == 2);
    if probe(&srv).is_none() {
        return;
    }

    let addr = srv.addr;
    let handles: Vec<_> = (0..4u32)
        .map(|_| {
            std::thread::spawn(move || {
                let mut pids = BTreeSet::new();
                for _ in 0..25 {
                    let (code, body) = http_get(addr, "/", REQ).expect("GET /");
                    assert_eq!(code, 200);
                    let body = String::from_utf8_lossy(&body).into_owned();
                    let pid: u32 = body
                        .rsplit_once(':')
                        .and_then(|(_, p)| p.parse().ok())
                        .unwrap_or_else(|| panic!("unexpected body {body:?}"));
                    pids.insert(pid);
                }
                pids
            })
        })
        .collect();
    let mut pids: BTreeSet<u32> = BTreeSet::new();
    for h in handles {
        match h.join() {
            Ok(p) => pids.extend(p),
            Err(e) => std::panic::resume_unwind(e),
        }
    }
    assert!(
        pids.len() >= 2,
        "4 concurrent clients against 2 workers must reach both\n{}",
        diagnostics(&srv)
    );

    let log_path = srv.dir.join("newrelic-agent.log");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        let all_present = pids.iter().all(|p| log.contains(&format!("({p} ")))
            && log.contains(&format!("({} ", srv.pid()));
        if all_present {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "agent log must show the master (MINIT) and every serving worker (late init); \
                 master {} workers {pids:?}\n--- {} ---\n{log}",
                srv.pid(),
                log_path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// With `newrelic.daemon.dont_launch = 3` the agent never forks the daemon, from MINIT
/// or RINIT. No worker may have a child, and the master's children are the workers only.
#[test]
fn newrelic_launches_no_daemon_under_the_pool() {
    let Some(srv) = newrelic_srv(2) else { return };
    let workers = wait_workers(&srv, BOOT_POOL, "2 workers", |p| p.len() == 2);
    if probe(&srv).is_none() {
        return;
    }
    for _ in 0..10 {
        let (code, _) = http_get(srv.addr, "/", REQ).expect("GET /");
        assert_eq!(code, 200);
    }
    for &w in &workers {
        assert!(
            worker_pids(w).is_empty(),
            "worker {w} must not have forked a daemon\n{}",
            diagnostics(&srv)
        );
    }
    assert_eq!(
        worker_pids(srv.pid()),
        workers,
        "the master's children must be the workers only\n{}",
        diagnostics(&srv)
    );
}

/// The agent swaps zend_error_cb at late init but chains to the original. rapira's
/// diagnostics use the SAPI log_message hook. The 500, the recovery, and the logged
/// message must all survive with the agent attached.
#[test]
fn newrelic_leaves_the_uncaught_throw_path_alone() {
    let Some(srv) = newrelic_srv(1) else { return };
    wait_workers(&srv, BOOT_POOL, "1 worker", |p| p.len() == 1);
    if probe(&srv).is_none() {
        return;
    }
    let (code, _) = http_get(srv.addr, "/?boom=1", REQ).expect("GET /?boom=1");
    assert_eq!(code, 500, "{}", diagnostics(&srv));
    let (code, body) = http_get(srv.addr, "/", REQ).expect("GET /");
    let body = String::from_utf8_lossy(&body);
    assert_eq!(
        code, 200,
        "the interpreter must survive the throw (got {body:?})"
    );
    nr_pid(&body, &srv);
    assert!(
        wait_log_contains(&srv, "newrelic-worker: uncaught", Duration::from_secs(5)),
        "the exception text must reach the server log\n{}",
        diagnostics(&srv)
    );
}

/// A pool with the Datadog tracer loaded for one test. The quieting keys are redundant
/// today: the SAPI check disables the tracer before sidecar, telemetry, or
/// remote-config setup. They stay so that a future upstream allowlist change starts no
/// network activity in CI. Env DD_* overrides these ini keys; the runners export none.
fn ddtrace_srv(processes: usize) -> Option<Server> {
    php_extension("ddtrace.so")?;
    let ini = format!(
        "extension_dir = \"{}\"\n\
         extension = ddtrace.so\n\
         datadog.trace.agent_url = \"http://127.0.0.1:9\"\n\
         datadog.trace.startup_logs = 0\n\
         datadog.instrumentation_telemetry_enabled = 0\n\
         datadog.remote_config_enabled = 0\n\
         datadog.trace.sidecar_connection_mode = subprocess\n\
         datadog.trace.log_level = error\n\
         display_errors = On\n\
         error_reporting = E_ALL\n\
         max_execution_time = 0\n\
         session.gc_probability = 0\n",
        php_extension_dir()?.display()
    );
    Some(spawn_with_phprc_and_config(
        "apm/ddtrace-worker.php",
        processes,
        &ini,
        "mode = \"worker\"\n",
    ))
}

fn dd_field<'a>(body: &'a str, key: &str) -> &'a str {
    body.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
        .unwrap_or_else(|| panic!("no {key} line in {body:?}"))
}

/// Neither of rapira's SAPI names is in the tracer's compatible-SAPI list, so MINIT
/// must disable tracing and still load the extension. When upstream adds rapira to
/// the list, this test fails with `enabled`. That failure is not a flake: rework the
/// suite then.
#[test]
fn ddtrace_self_disables_under_the_rapira_sapi() {
    let Some(srv) = ddtrace_srv(2) else { return };
    wait_workers(&srv, BOOT_POOL, "2 workers", |p| p.len() == 2);
    let Some(body) = probe(&srv) else { return };
    assert!(
        ["fastcgi", "rapira"].contains(&dd_field(&body, "dd:sapi")),
        "unexpected SAPI name (got {body:?})"
    );
    assert!(
        !dd_field(&body, "dd:version").is_empty(),
        "phpversion('ddtrace') must be set: the extension really loaded (got {body:?})"
    );
    assert_eq!(
        dd_field(&body, "dd:tracing"),
        "disabled",
        "phpinfo's \"Datadog tracing support\" row renders the disable flag (got {body:?})"
    );
    assert_eq!(
        dd_field(&body, "dd:active_span"),
        "NULL",
        "no root span may exist while the tracer is disabled (got {body:?})"
    );
}

/// The disabled tracer must stay disabled across requests and workers, keep the pool
/// serving, and start no sidecar process.
#[test]
fn ddtrace_keeps_the_pool_serving_and_forks_nothing() {
    let Some(srv) = ddtrace_srv(2) else { return };
    let workers = wait_workers(&srv, BOOT_POOL, "2 workers", |p| p.len() == 2);
    if probe(&srv).is_none() {
        return;
    }
    for _ in 0..50 {
        let (code, body) = http_get(srv.addr, "/", REQ).expect("GET /");
        let body = String::from_utf8_lossy(&body);
        assert_eq!(code, 200, "got {body:?}\n{}", diagnostics(&srv));
        assert_eq!(
            dd_field(&body, "dd:tracing"),
            "disabled",
            "the disabled state must not change after the fork (got {body:?})"
        );
    }
    for &w in &workers {
        assert!(
            worker_pids(w).is_empty(),
            "worker {w} must not have started a sidecar\n{}",
            diagnostics(&srv)
        );
    }
    assert_eq!(
        worker_pids(srv.pid()),
        workers,
        "the master's children must be the workers only\n{}",
        diagnostics(&srv)
    );
}
