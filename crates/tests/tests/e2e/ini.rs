//! php.ini discovery: the process working directory must not be part of it.

use crate::harness::{self, http_get};
use std::time::Duration;

const REQ: Duration = Duration::from_secs(10);

#[test]
fn php_ini_in_the_working_directory_is_ignored() {
    let srv = harness::spawn_in_cwd("ini/precision.php", 1, "precision = 5\n");
    let (code, body) = http_get(srv.addr, "/", REQ).expect("request");
    assert_eq!(code, 200, "{}", harness::diagnostics(&srv));
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains("precision=14"),
        "a php.ini in the cwd must not apply (got {body:?})\n{}",
        harness::diagnostics(&srv)
    );
}

#[test]
fn the_same_file_applies_through_phprc() {
    // Control: without it a fixture that never reached the ini at all would let
    // the test above pass vacuously.
    let srv = harness::spawn_in_cwd_with_phprc("ini/precision.php", 1, "precision = 5\n");
    let (code, body) = http_get(srv.addr, "/", REQ).expect("request");
    assert_eq!(code, 200, "{}", harness::diagnostics(&srv));
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains("precision=5"),
        "PHPRC must apply (got {body:?})\n{}",
        harness::diagnostics(&srv)
    );
}
