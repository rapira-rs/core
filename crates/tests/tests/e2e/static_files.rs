use crate::harness::{
    Conn, diagnostics, http_get, http_get_raw, http_post, http_raw_bytes, scratch_dir,
    spawn_boot_failure, spawn_with_http_extra,
};
use std::time::Duration;

const T: Duration = Duration::from_secs(10);

fn static_root(files: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = scratch_dir();
    for (name, contents) in files {
        std::fs::write(dir.join(name), contents).expect("write static file");
    }
    dir
}

/// Reads one field from the response head. A field name can use any letter case, so the
/// comparison ignores the case.
fn head_field(raw: &[u8], name: &str) -> String {
    let text = String::from_utf8_lossy(raw);
    let head = text.split("\r\n\r\n").next().unwrap_or_default();
    head.lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_default()
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
        &format!(
            "middleware = [\"static\"]\n[http.static]\nroot = \"{}\"\n",
            root.display()
        ),
    );

    let raw = http_get_raw(srv.addr, "/app.css", &[], T).expect("GET /app.css");
    let text = String::from_utf8_lossy(&raw);
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "{text}\n{}",
        diagnostics(&srv)
    );
    let head = text.split("\r\n\r\n").next().expect("head").to_lowercase();
    assert!(head.contains("\r\ncontent-type: text/css\r\n"), "{head}");
    assert!(head.contains("\r\ncontent-length: 15\r\n"), "{head}");
    assert!(text.ends_with("body{color:red}"), "{text}");

    let (code, body) = http_get(srv.addr, "/nope", T).expect("GET /nope");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert!(body.starts_with(b"ok:"), "a miss must reach php");

    let (code, body) =
        http_post(srv.addr, "/nope", b"text/plain", b"payload", T).expect("POST /nope");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    assert!(body.starts_with(b"ok:"), "a post must reach php");

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

/// The client sees no sign of the cache. A second request gives the same validators. A
/// conditional request gives 304 from the entry. A new version of the file appears after the
/// freshness window ends.
#[test]
fn cached_static_files_revalidate_over_the_wire() {
    let root = static_root(&[("app.css", "body{color:red}")]);
    let srv = spawn_with_http_extra(
        "shared/echo-worker.php",
        1,
        &format!(
            "middleware = [\"static\"]\n[http.static]\nroot = \"{}\"\n",
            root.display()
        ),
    );

    let first = http_get_raw(srv.addr, "/app.css", &[], T).expect("GET /app.css");
    let etag = head_field(&first, "etag");
    let modified = head_field(&first, "last-modified");
    assert!(!etag.is_empty(), "no etag\n{}", diagnostics(&srv));

    let second = http_get_raw(srv.addr, "/app.css", &[], T).expect("second GET");
    assert_eq!(head_field(&second, "etag"), etag);
    assert_eq!(head_field(&second, "last-modified"), modified);
    assert_eq!(head_field(&second, "content-type"), "text/css");
    assert_eq!(head_field(&second, "content-length"), "15");
    let text = String::from_utf8_lossy(&second);
    assert!(text.ends_with("body{color:red}"), "{text}");

    let raw = http_get_raw(srv.addr, "/app.css", &[("If-None-Match", &etag)], T).expect("etag GET");
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("HTTP/1.1 304"), "{text}");
    assert!(text.ends_with("\r\n\r\n"), "a 304 has no body: {text}");

    let raw = http_get_raw(srv.addr, "/app.css", &[("If-Modified-Since", &modified)], T)
        .expect("date GET");
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("HTTP/1.1 304"), "{text}");

    let head = http_raw_bytes(
        srv.addr,
        b"HEAD /app.css HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        T,
    )
    .expect("HEAD /app.css");
    let text = String::from_utf8_lossy(&head);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert_eq!(head_field(&head, "content-length"), "15");
    assert!(
        text.ends_with("\r\n\r\n"),
        "a HEAD response has no body: {text}"
    );

    // The new file has a different length and a different mtime. Either one is sufficient to
    // cause the reload.
    let path = root.join("app.css");
    std::fs::write(&path, "body{color:lime}").expect("rewrite");
    std::fs::File::options()
        .write(true)
        .open(&path)
        .expect("open for mtime")
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_600_000_000))
        .expect("set mtime");
    std::thread::sleep(Duration::from_millis(1100));

    let raw = http_get_raw(srv.addr, "/app.css", &[], T).expect("GET after the rewrite");
    let text = String::from_utf8_lossy(&raw);
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    assert_eq!(head_field(&raw, "content-length"), "16");
    assert_ne!(head_field(&raw, "etag"), etag);
    assert!(text.ends_with("body{color:lime}"), "{text}");

    let _ = std::fs::remove_dir_all(&root);
}

/// One connection serves sequential requests through the middleware chain:
/// miss to PHP, static hit, the same file from the cache, miss to PHP again.
#[test]
fn the_chain_serves_sequential_requests_on_one_connection() {
    let root = static_root(&[("app.css", "body{}")]);
    let srv = spawn_with_http_extra(
        "shared/echo-worker.php",
        1,
        &format!(
            "middleware = [\"static\"]\n[http.static]\nroot = \"{}\"\n",
            root.display()
        ),
    );
    let mut c = Conn::open(srv.addr, T).expect("connect");

    fn content_length(fields: &[(String, String)]) -> usize {
        fields
            .iter()
            .find(|(k, _)| k == "content-length")
            .and_then(|(_, v)| v.parse().ok())
            .expect("content-length header")
    }

    c.send(b"GET /nope HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("first request");
    let (status, fields) = c.read_head(T).expect("first head");
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
    let body = c.read_n(content_length(&fields), T).expect("first body");
    assert!(body.starts_with(b"ok:"), "first body must come from php");

    c.send(b"GET /app.css HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("second request");
    let (status, fields) = c.read_head(T).expect("second head");
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
    let body = c.read_n(content_length(&fields), T).expect("second body");
    assert_eq!(body.as_slice(), b"body{}", "the hit must serve the file");

    // The cache answers this request. A wrong content-length breaks the connection here.
    c.send(b"GET /app.css HTTP/1.1\r\nHost: e2e\r\n\r\n")
        .expect("cached request");
    let (status, fields) = c.read_head(T).expect("cached head");
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
    let body = c.read_n(content_length(&fields), T).expect("cached body");
    assert_eq!(body.as_slice(), b"body{}", "the cache must serve the file");

    c.send(b"GET /nope HTTP/1.1\r\nHost: e2e\r\nConnection: close\r\n\r\n")
        .expect("third request on the same connection");
    let (status, fields) = c.read_head(T).expect("reused connection must serve");
    assert_eq!(status, 200, "\n{}", diagnostics(&srv));
    let body = c.read_n(content_length(&fields), T).expect("third body");
    assert!(body.starts_with(b"ok:"), "third body must come from php");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_missing_static_root_refuses_to_boot() {
    let (status, log) = spawn_boot_failure(
        "shared/echo-worker.php",
        "middleware = [\"static\"]\n[http.static]\nroot = \"/nonexistent-rapira-static-root\"\n",
    );
    assert_eq!(status.code(), Some(1), "{log}");
    assert!(
        log.contains("http.static.root") && log.contains("is not accessible"),
        "{log}"
    );
}

/// A 000 directory passes a stat of its own path; the boot probe must resolve inside it.
#[test]
fn an_unreadable_static_root_refuses_to_boot() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch_dir();
    let root = dir.join("root");
    std::fs::create_dir(&root).expect("create root");
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).expect("chmod root");
    // Root bypasses permission checks; the case cannot occur for that user.
    if std::fs::read_dir(&root).is_ok() {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let (status, log) = spawn_boot_failure(
        "shared/echo-worker.php",
        &format!(
            "middleware = [\"static\"]\n[http.static]\nroot = \"{}\"\n",
            root.display()
        ),
    );
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).expect("restore root");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(status.code(), Some(1), "{log}");
    assert!(
        log.contains("http.static.root") && log.contains("is not accessible"),
        "{log}"
    );
}

/// A 0400 root lists but cannot resolve child paths; boot must require search permission.
#[test]
fn a_static_root_without_search_permission_refuses_to_boot() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch_dir();
    let root = dir.join("root");
    std::fs::create_dir(&root).expect("create root");
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o400)).expect("chmod root");
    // Root bypasses permission checks; the case cannot occur for that user.
    if std::fs::metadata(root.join(".")).is_ok() {
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let (status, log) = spawn_boot_failure(
        "shared/echo-worker.php",
        &format!(
            "middleware = [\"static\"]\n[http.static]\nroot = \"{}\"\n",
            root.display()
        ),
    );
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).expect("restore root");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(status.code(), Some(1), "{log}");
    assert!(
        log.contains("http.static.root") && log.contains("is not accessible"),
        "{log}"
    );
}

#[test]
fn a_static_root_that_is_not_a_directory_refuses_to_boot() {
    let dir = scratch_dir();
    let file = dir.join("root");
    std::fs::write(&file, "x").expect("write file root");
    let (status, log) = spawn_boot_failure(
        "shared/echo-worker.php",
        &format!(
            "middleware = [\"static\"]\n[http.static]\nroot = \"{}\"\n",
            file.display()
        ),
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(status.code(), Some(1), "{log}");
    assert!(log.contains("is not a directory"), "{log}");
}
