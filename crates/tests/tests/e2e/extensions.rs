use std::collections::HashMap;
use std::time::Duration;

use crate::harness::{
    diagnostics, fan_out, fixture_path, http_get, http_get_with_headers, spawn_with_config,
    spawn_with_phprc_and_config, wait_workers,
};

const REQ: Duration = Duration::from_secs(10);

fn rng_draw(body: &str) -> (u32, String) {
    let (pid, first) = body
        .strip_prefix("pid=")
        .and_then(|r| r.split_once(" first="))
        .unwrap_or_else(|| panic!("unexpected body {body:?}"));
    let pid: u32 = pid
        .parse()
        .unwrap_or_else(|_| panic!("bad pid in {body:?}"));
    (pid, first.to_owned())
}

/// No two workers may emit identical first draws. The master runs no PHP, so no DRBG
/// state exists at fork time and each worker seeds its own; the cached draw also pins
/// that the entrypoint's top level runs once per worker. random_bytes() uses
/// ext/random's getrandom(2) path, never OpenSSL, and this test does not cover it.
#[test]
fn forked_workers_do_not_share_the_openssl_drbg() {
    let srv = spawn_with_config("extensions/openssl-rng-worker.php", 4, "");
    let workers = wait_workers(&srv, Duration::from_secs(20), "4 workers", |p| p.len() == 4);

    let mut per_pid: HashMap<u32, String> = HashMap::new();
    for _ in 0..5 {
        for (pid, first) in fan_out(srv.addr, "/", 4, 10, rng_draw) {
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
        if per_pid.len() == workers.len() {
            break;
        }
    }
    assert_eq!(
        per_pid.len(),
        workers.len(),
        "every worker must answer at least once\n{}",
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

fn browscap_ini() -> String {
    format!(
        "browscap = \"{}\"\ndisplay_errors = On\nerror_reporting = E_ALL\nmax_execution_time = 0\nsession.gc_probability = 0\n",
        fixture_path("browscap/rapira-browscap.ini").display()
    )
}

/// The `browscap` ini is PHP_INI_SYSTEM. MINIT reads the file once into persistent
/// memory. Under rapira the parse runs in the master, before the fork, and the
/// workers share the pages copy-on-write.
fn browscap_probe(ua: &str) -> String {
    let srv = spawn_with_phprc_and_config(
        "browscap/browscap-worker.php",
        1,
        &browscap_ini(),
        "mode = \"worker\"\n",
    );
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
    let (code, body) =
        http_get_with_headers(srv.addr, "/", &[("User-Agent", ua)], REQ).expect("GET /");
    assert_eq!(code, 200, "UA {ua:?}\n{}", diagnostics(&srv));
    String::from_utf8_lossy(&body).into_owned()
}

/// Two patterns match. browser_reg_compare keeps the one with the most non-wildcard
/// characters. The parent fills only the keys the winner omits. The regex is the
/// lowercased pattern with `*` -> `.*`, `?` -> `.` and the specials escaped
/// (browscap_convert_pattern); the true/false value mapping is stated in the fixture ini.
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

/// This user agent is shorter than the Bot pattern's literal minimum, so browscap's
/// length check discards that entry before any comparison. The shorter `Rapira/1.0*`
/// entry wins, and its own keys override the parent's.
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

/// get_browser(null) reads $_SERVER['HTTP_USER_AGENT']; without the header it must
/// warn and return false. This pins that the SAPI does not invent a user agent.
#[test]
fn get_browser_without_a_user_agent_returns_false() {
    let srv = spawn_with_phprc_and_config(
        "browscap/browscap-worker.php",
        1,
        &browscap_ini(),
        "mode = \"worker\"\n",
    );
    wait_workers(&srv, Duration::from_secs(20), "1 worker", |p| p.len() == 1);
    let (code, body) = http_get(srv.addr, "/", REQ).expect("GET /");
    let body = String::from_utf8_lossy(&body);
    assert_eq!(code, 200, "{}", diagnostics(&srv));
    assert!(
        body.contains("HTTP_USER_AGENT") && body.contains("browscap:false"),
        "expected the warning and a false return (got {body:?})"
    );
}

fn table_and_pid(body: &str) -> (String, u32) {
    let (table, pid) = body
        .rsplit_once("\npid=")
        .unwrap_or_else(|| panic!("no pid line in {body:?}"));
    (
        table.to_owned(),
        pid.parse()
            .unwrap_or_else(|_| panic!("bad pid in {body:?}")),
    )
}

/// The browscap table parses once in the master at MINIT and every worker serves from
/// the same persistent table: identical answers from distinct pids, request after request.
#[test]
fn browscap_table_is_shared_and_stable_across_workers() {
    let srv = spawn_with_phprc_and_config(
        "browscap/browscap-worker.php",
        2,
        &browscap_ini(),
        "mode = \"worker\"\n",
    );
    let workers = wait_workers(&srv, Duration::from_secs(20), "2 workers", |p| p.len() == 2);
    let mut tables: Vec<String> = Vec::new();
    let mut pids: Vec<u32> = Vec::new();
    for _ in 0..5 {
        for (table, pid) in fan_out(srv.addr, "/?probe=pid", 2, 10, table_and_pid) {
            tables.push(table);
            pids.push(pid);
        }
        pids.sort_unstable();
        pids.dedup();
        if pids.len() == workers.len() {
            break;
        }
    }
    assert_eq!(
        pids.len(),
        workers.len(),
        "both workers must answer\n{}",
        diagnostics(&srv)
    );
    tables.sort();
    tables.dedup();
    assert_eq!(
        tables.len(),
        1,
        "every worker must serve the identical table (got {} variants)",
        tables.len()
    );
}
