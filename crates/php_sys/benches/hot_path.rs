//! Benchmarks for the pure-Rust hot paths of `php_sys` that run without a live
//! PHP runtime:
//!
//! * [`ReqC::build`] — turns an incoming [`Request`] into the C-string bundle
//!   handed to PHP on every request (header folding, `CString` conversions,
//!   server-var map build). This is per-request work on the critical path.
//! * [`Scoreboard`] — the lock-free per-worker counters updated on every handled
//!   request and aggregated into a snapshot for status reporting.

use std::io;

use divan::{Bencher, black_box};
use php_sys::scoreboard::{Event, Scoreboard, sb_set, sb_update};
use php_sys::types::{ReqC, Request};

fn main() {
    divan::main();
}

/// Build a representative request with `header_count` request headers and
/// `server_var_count` server variables — the shape `ReqC::build` sees per hit.
fn sample_request(header_count: usize, server_var_count: usize) -> Request {
    let mut headers: Vec<(String, Vec<u8>)> = vec![
        ("Host".to_string(), b"example.com".to_vec()),
        (
            "User-Agent".to_string(),
            b"Mozilla/5.0 (rapira-bench)".to_vec(),
        ),
        ("Accept".to_string(), b"text/html,application/json".to_vec()),
        (
            "Authorization".to_string(),
            b"Bearer abcdef0123456789".to_vec(),
        ),
        ("Cookie".to_string(), b"session=abc123".to_vec()),
        ("Cookie".to_string(), b"theme=dark; lang=en".to_vec()),
    ];
    for i in 0..header_count {
        headers.push((
            format!("X-Custom-Header-{i}"),
            format!("value-{i}").into_bytes(),
        ));
    }

    let mut server_vars: Vec<(String, String)> = vec![
        ("SERVER_SOFTWARE".to_string(), "rapira".to_string()),
        ("GATEWAY_INTERFACE".to_string(), "CGI/1.1".to_string()),
    ];
    for i in 0..server_var_count {
        server_vars.push((format!("HTTP_X_VAR_{i}"), format!("v{i}")));
    }

    Request {
        method: "POST".to_string(),
        uri: "/api/v1/users/42/profile".to_string(),
        https: true,
        query: "expand=details&fields=id,name,email".to_string(),
        protocol: "HTTP/1.1".to_string(),
        remote_addr: "203.0.113.7".to_string(),
        server_name: "example.com".to_string(),
        server_port: "443".to_string(),
        remote_port: "51234".to_string(),
        script_name: "/index.php".to_string(),
        document_root: "/var/www/html".to_string(),
        script_filename: "/var/www/html/index.php".into(),
        headers,
        server_vars,
        content_type: Some("application/json".to_string()),
        content_length: 128,
        body: Box::new(io::empty()),
    }
}

#[divan::bench(args = [(6, 2), (24, 16), (64, 48)])]
fn reqc_build(bencher: Bencher, sizes: (usize, usize)) {
    let (headers, server_vars) = sizes;
    let req = sample_request(headers, server_vars);
    bencher.bench_local(|| ReqC::build(black_box(&req)));
}

#[divan::bench(args = [8, 64, 256])]
fn scoreboard_update(bencher: Bencher, requests: usize) {
    // `sb_update` is the per-request update: it resolves the thread-local worker
    // slot registered by `sb_set` and bumps the relevant atomic counter.
    let board = Scoreboard::new(16);
    sb_set(0, board);
    bencher.bench_local(|| {
        for i in 0..requests {
            sb_update(Event::Handled(black_box(i) % 7 == 0));
        }
    });
}

#[divan::bench(args = [8, 64, 256])]
fn scoreboard_snapshot(bencher: Bencher, workers: usize) {
    // Populate every worker slot through the public per-request API, then measure
    // the aggregation done by `snapshot` (status/health reporting path).
    let board = Scoreboard::new(workers);
    for id in 0..workers {
        sb_set(id, board.clone());
        sb_update(Event::Handled(id % 3 == 0));
        sb_update(Event::Recycled);
        if id % 5 == 0 {
            sb_update(Event::Unhealthy);
        }
    }
    bencher.bench_local(|| black_box(&board).snapshot());
}
