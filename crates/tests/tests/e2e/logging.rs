use crate::harness::*;
use std::time::{Duration, Instant};

/// `[log] format = "json"` must shape every runtime record while the harness's
/// hardcoded `RUST_LOG=info` keeps governing the filter: the config owns the
/// format, the env owns the filter.
#[test]
fn json_format_shapes_the_log() {
    let srv = spawn_with_config("echo-worker.php", 1, "[log]\nformat = \"json\"\n");
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
                    && v["message"]
                        .as_str()
                        .is_some_and(|m| m.starts_with("rapira_core v"))
            })
        });
        // Negative shape: env_logger's plain look is "[<ts> LEVEL target] msg".
        let plain = text
            .lines()
            .any(|l| l.starts_with('[') && l.as_bytes().get(1).is_some_and(u8::is_ascii_digit));
        assert!(!plain, "plain-format line in json mode:\n{text}");
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
