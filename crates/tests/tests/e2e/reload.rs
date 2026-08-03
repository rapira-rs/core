use crate::harness::*;
use std::time::Duration;

#[test]
#[ignore = "pending the dispatcher API (worker mode serves no requests)"]
fn usr2_reload_zero_downtime() {
    let srv = spawn_with_config("shared/echo-worker.php", 3, "");
    let pids0 = wait_workers(&srv, Duration::from_secs(20), "3 workers", |p| p.len() == 3);
    let storm = storm(srv.addr, 8);
    std::thread::sleep(Duration::from_millis(500)); // warm up under load
    signal(srv.pid(), libc::SIGUSR2);
    wait_workers(
        &srv,
        Duration::from_secs(60),
        "reload to 3 disjoint pids",
        |p| p.len() == 3 && p.iter().all(|x| !pids0.contains(x)),
    );
    std::thread::sleep(Duration::from_secs(2)); // keep hammering the new generation
    let tally = storm.halt();

    // The listener stays open across the reload: no connection is ever refused.
    assert_eq!(
        tally.refused,
        0,
        "reload refused connections; last error: {:?}\n{}",
        tally.last_err,
        diagnostics(&srv),
    );
    // Traffic flows throughout the generation swap.
    assert!(tally.ok > 0, "storm served no requests during the reload");
    // Accept-race drops are bounded by workers cycled (3) plus the overlap
    // headroom worker; a wholesale outage would blow past this.
    assert!(
        tally.truncated <= 6,
        "reload dropped {} in-flight requests; last error: {:?}\n{}",
        tally.truncated,
        tally.last_err,
        diagnostics(&srv),
    );
}
