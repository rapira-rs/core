use crate::harness::*;
use std::time::{Duration, Instant};

/// `[log] format = "json"` must shape every runtime record while the harness's
/// hardcoded `RUST_LOG=info` keeps governing the filter: the config owns the
/// format, the env owns the filter.
#[test]
fn json_format_shapes_the_log() {
    let srv = spawn_with_config("shared/echo-worker.php", 1, "[log]\nformat = \"json\"\n");
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));

    // The banner is written before the listener binds, so it is on disk by the
    // time the readiness probe connected; the bounded retry only absorbs fs lag.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let text = std::fs::read_to_string(srv.dir.join("server.log")).unwrap_or_default();
        // Positive shape: the boot banner as a parsed JSON record.
        let banner = text.lines().any(|l| {
            serde_json::from_str::<serde_json::Value>(l).is_ok_and(|v| {
                v["target"] == "rapira"
                    && v["fields"]["message"]
                        .as_str()
                        .is_some_and(|m| m.starts_with("rapira_core v"))
            })
        });

        // Negative shape: every complete line is one JSON object. A trailing
        // partial line (a write in flight) is the only exclusion.
        let complete = &text[..text.rfind('\n').map_or(0, |i| i + 1)];
        for l in complete.lines().filter(|l| !l.is_empty()) {
            assert!(
                serde_json::from_str::<serde_json::Value>(l).is_ok(),
                "non-JSON line in json mode: {l}\n{}",
                diagnostics(&srv)
            );
        }
        if banner {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no JSON banner line in server.log\n{}",
            diagnostics(&srv)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Default `[log]`: the plain format on a redirected (non-tty) stderr keeps the
/// human shape and emits no ANSI escapes.
#[test]
fn plain_format_is_uncolored_when_redirected() {
    let srv = spawn_with_config("shared/echo-worker.php", 1, "");
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let text = std::fs::read_to_string(srv.dir.join("server.log")).unwrap_or_default();
        assert!(
            !text.contains('\u{1b}'),
            "ANSI escape in a redirected log:\n{text}"
        );
        if text
            .lines()
            .any(|l| l.contains("INFO") && l.contains("rapira_core v"))
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "no plain banner line in server.log\n{}",
            diagnostics(&srv)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `[log.targets] php = "warn"` is what makes PHP diagnostics visible;
/// `level = "error"` alone hides them. Runs without `RUST_LOG` so the config
/// owns the filter.
#[test]
fn log_targets_php_restores_php_diagnostics() {
    let srv = spawn_without_rust_log("logging/warn-worker.php", 1, "[log]\nlevel = \"error\"\n");
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    // The diagnostic is written during request handling; a short settle covers
    // the teardown-path record before asserting absence.
    std::thread::sleep(Duration::from_millis(300));
    let text = std::fs::read_to_string(srv.dir.join("server.log")).unwrap_or_default();
    assert!(
        !text.contains("WARN-MARK"),
        "php warning leaked past level = \"error\":\n{text}"
    );
    drop(srv);

    let srv = spawn_without_rust_log(
        "logging/warn-worker.php",
        1,
        "[log]\nlevel = \"error\"\n[log.targets]\nphp = \"warn\"\n",
    );
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let text = std::fs::read_to_string(srv.dir.join("server.log")).unwrap_or_default();
        if text.contains("WARN-MARK") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "php = \"warn\" did not surface the diagnostic\n{}",
            diagnostics(&srv)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `RUST_LOG` replaces the configured filter wholesale — the one env-beats-config
/// knob. The same `level = "error"` that hides the PHP warning in
/// [`log_targets_php_restores_php_diagnostics`] lets it through under `RUST_LOG=info`.
#[test]
fn rust_log_replaces_the_config_filter() {
    let srv = spawn_with_config("logging/warn-worker.php", 1, "[log]\nlevel = \"error\"\n");
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let text = std::fs::read_to_string(srv.dir.join("server.log")).unwrap_or_default();
        if text.contains("WARN-MARK") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "RUST_LOG=info did not override level = \"error\"\n{}",
            diagnostics(&srv)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
