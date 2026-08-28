use crate::harness::{
    diagnostics, http_get, http_get_raw, spawn_boot_failure, spawn_with_http_extra,
};
use std::time::Duration;

const T: Duration = Duration::from_secs(10);

fn static_root(files: &[(&str, &str)]) -> std::path::PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rapira-static-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create static root");
    for (name, contents) in files {
        std::fs::write(dir.join(name), contents).expect("write static file");
    }
    dir
}

/// One boot covers the wire behavior: a hit with its headers, a miss into PHP, the source and dotfile guards, and a range.
#[test]
fn static_files_serve_over_the_wire() {
    let root = static_root(&[
        ("app.css", "body{color:red}"),
        ("big.bin", "0123456789"),
        ("index.php", "<?php leak();"),
        (".env", "SECRET=1"),
    ]);
    let srv = spawn_with_http_extra(
        "shared/echo-worker.php",
        1,
        &format!("[http.static]\nroot = \"{}\"\n", root.display()),
    );

    let (code, body) = http_get(srv.addr, "/app.css", T).expect("GET /app.css");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert_eq!(body, b"body{color:red}");
    let raw = http_get_raw(srv.addr, "/app.css", &[], T).expect("raw GET /app.css");
    let head = String::from_utf8_lossy(&raw).to_lowercase();
    assert!(head.contains("content-type: text/css"), "{head}");
    assert!(head.contains("content-length: 15"), "{head}");

    let (code, body) = http_get(srv.addr, "/nope", T).expect("GET /nope");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert!(body.starts_with(b"ok:"), "a miss must reach php");

    for path in ["/index.php", "/.env"] {
        let (_, body) = http_get(srv.addr, path, T).expect(path);
        assert!(
            body.starts_with(b"ok:"),
            "{path} must reach php, got {}",
            String::from_utf8_lossy(&body)
        );
    }

    let raw = http_get_raw(srv.addr, "/big.bin", &[("Range", "bytes=0-3")], T).expect("range GET");
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("HTTP/1.1 206"), "{text}");
    assert!(
        text.to_lowercase().contains("content-range: bytes 0-3/10"),
        "{text}"
    );
    assert!(text.ends_with("0123"), "{text}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_static_root_refuses_to_boot() {
    let (status, log) = spawn_boot_failure(
        "shared/echo-worker.php",
        "[http.static]\nroot = \"/nonexistent-rapira-static-root\"\n",
    );
    assert!(!status.success(), "{log}");
    assert!(log.contains("http.static.root"), "{log}");
}
