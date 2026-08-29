use crate::harness::{
    diagnostics, http_get, http_get_raw, http_post, scratch_dir, spawn_boot_failure,
    spawn_with_http_extra,
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
