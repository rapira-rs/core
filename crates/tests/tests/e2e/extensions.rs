use std::collections::HashMap;
use std::time::Duration;

use crate::harness::{
    diagnostics, fixture_path, http_get, http_get_with_headers, spawn_with_config,
    spawn_with_phprc_and_config, wait_workers,
};

const REQ: Duration = Duration::from_secs(10);

/// OpenSSL 1.1.1d and newer reseed the DRBG when the fork id changes
/// (providers/implementations/rands/drbg.c). PHP does nothing on fork. The first
/// openssl_random_pseudo_bytes() of two workers forked from one master must therefore
/// differ. random_bytes() uses ext/random's getrandom(2) path, never OpenSSL, and
/// stays outside this pin.
#[test]
fn forked_workers_do_not_share_the_openssl_drbg() {
    let srv = spawn_with_config("extensions/openssl-rng-worker.php", 4, "");
    wait_workers(&srv, Duration::from_secs(20), "4 workers", |p| p.len() == 4);

    let addr = srv.addr;
    let draws: Vec<(u32, String)> = {
        let handles: Vec<_> = (0..4u32)
            .map(|_| {
                std::thread::spawn(move || {
                    let mut seen = Vec::new();
                    for _ in 0..25 {
                        let (code, body) = http_get(addr, "/", REQ).expect("GET /");
                        assert_eq!(code, 200);
                        let body = String::from_utf8_lossy(&body).into_owned();
                        let (pid, first) = body
                            .strip_prefix("pid=")
                            .and_then(|r| r.split_once(" first="))
                            .unwrap_or_else(|| panic!("unexpected body {body:?}"));
                        let pid: u32 = pid
                            .parse()
                            .unwrap_or_else(|_| panic!("bad pid in {body:?}"));
                        seen.push((pid, first.to_owned()));
                    }
                    seen
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| match h.join() {
                Ok(v) => v,
                Err(e) => std::panic::resume_unwind(e),
            })
            .collect()
    };

    let mut per_pid: HashMap<u32, String> = HashMap::new();
    for (pid, first) in draws {
        assert!(
            first.len() == 32
                && first
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "bin2hex of 16 bytes must be 32 lowercase hex chars (got {first:?})"
        );
        match per_pid.get(&pid) {
            Some(prev) => assert_eq!(
                prev, &first,
                "a worker's cached first draw must not change (pid {pid})"
            ),
            None => {
                per_pid.insert(pid, first);
            }
        }
    }
    assert!(
        per_pid.len() >= 2,
        "4 concurrent clients against 4 workers must reach at least two workers\n{}",
        diagnostics(&srv)
    );
    let mut firsts: Vec<&String> = per_pid.values().collect();
    firsts.sort_unstable();
    firsts.dedup();
    assert_eq!(
        firsts.len(),
        per_pid.len(),
        "distinct workers must draw distinct first bytes (per pid: {per_pid:?})"
    );
}

/// The `browscap` ini is PHP_INI_SYSTEM. MINIT reads the file once into persistent
/// memory. Under rapira the parse runs in the master, before the fork, and the
/// workers share the pages copy-on-write.
fn browscap_probe(ua: &str) -> String {
    let ini = format!(
        "browscap = \"{}\"\ndisplay_errors = On\nerror_reporting = E_ALL\nmax_execution_time = 0\nsession.gc_probability = 0\n",
        fixture_path("browscap/rapira-browscap.ini").display()
    );
    let srv = spawn_with_phprc_and_config(
        "browscap/browscap-worker.php",
        1,
        &ini,
        "mode = \"worker\"\n",
    );
    let (code, body) =
        http_get_with_headers(srv.addr, "/", &[("User-Agent", ua)], REQ).expect("GET /");
    assert_eq!(code, 200, "UA {ua:?}\n{}", diagnostics(&srv));
    String::from_utf8_lossy(&body).into_owned()
}

/// Two patterns match. browser_reg_compare keeps the one with the most non-wildcard
/// characters. The parent fills only the keys the winner omits. true/false parse to
/// "1"/"" (browscap.c:317-331). The regex is the lowercased pattern with `*` -> `.*`,
/// `?` -> `.` and the specials escaped (browscap_convert_pattern).
#[test]
fn get_browser_picks_the_longest_pattern_and_inherits_the_parent() {
    let body = browscap_probe("Rapira/1.0 (Linux x86_64) Bot/9");
    let expected = "browser=RapiraBot\n\
                    platform=unknown\n\
                    crawler=1\n\
                    version=1.0\n\
                    parent=RapiraBase\n\
                    browser_name_pattern=Rapira/1.0 (Linux*) Bot*\n\
                    browser_name_regex=~^rapira/1\\.0 \\(linux.*\\) bot.*$~";
    assert_eq!(body, expected);
}

/// The Bot pattern fails on the literal prefix ("linux" vs "darwi"). The shorter
/// `Rapira/1.0*` entry wins, and its own keys override the parent's.
#[test]
fn get_browser_falls_through_to_the_less_specific_pattern() {
    let body = browscap_probe("Rapira/1.0 (Darwin)");
    let expected = "browser=RapiraProbe\n\
                    platform=unknown\n\
                    crawler=\n\
                    version=1.0\n\
                    parent=RapiraBase\n\
                    browser_name_pattern=Rapira/1.0*\n\
                    browser_name_regex=~^rapira/1\\.0.*$~";
    assert_eq!(body, expected);
}

/// No pattern matches, so the literal "Default Browser Capability Settings" entry
/// (DEFAULT_SECTION_NAME in browscap.c) answers. It has no Parent, so no parent key.
#[test]
fn get_browser_falls_back_to_the_default_section() {
    let body = browscap_probe("curl/8.5.0");
    let expected = "browser=RapiraDefault\n\
                    platform=unknown\n\
                    crawler=\n\
                    version=0\n\
                    parent=<unset>\n\
                    browser_name_pattern=Default Browser Capability Settings\n\
                    browser_name_regex=~^default browser capability settings$~";
    assert_eq!(body, expected);
}
