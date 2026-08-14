//! Concurrent producers against the dispatcher: every request is unique and
//! its response predictable, so any cross-unit mixing is a wrong body.

use std::time::Duration;

use crate::harness::{http_get, spawn_with_config, wait_workers};

/// 8 producer threads × 50 unique requests over a 4-worker dispatcher pool;
/// the fixture answers `n` with `n+1` from inside a per-unit fiber.
#[test]
fn concurrent_unique_requests_never_mix() {
    let srv = spawn_with_config("lifecycle/fiber-worker.php", 4, "");
    wait_workers(&srv, Duration::from_secs(20), "4 workers", |p| p.len() == 4);

    let addr = srv.addr;
    let producers: Vec<_> = (0..8u32)
        .map(|t| {
            std::thread::spawn(move || {
                for i in 0..50u32 {
                    let n = t * 1000 + i;
                    let (code, body) = http_get(addr, &format!("/?n={n}"), Duration::from_secs(10))
                        .unwrap_or_else(|e| panic!("GET /?n={n}: {e}"));
                    assert_eq!(code, 200, "n={n}");
                    assert_eq!(
                        String::from_utf8_lossy(&body),
                        format!("r={}", n + 1),
                        "response must answer exactly its own request (n={n})"
                    );
                }
            })
        })
        .collect();
    for p in producers {
        if let Err(e) = p.join() {
            std::panic::resume_unwind(e);
        }
    }
}
