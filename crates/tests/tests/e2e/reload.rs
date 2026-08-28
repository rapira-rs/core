use crate::harness::*;
use std::time::Duration;

/// USR2 replaces every worker while the master keeps the listener bound; connection drops during the swap are the load balancer's concern, served responses are ours.
#[test]
fn usr2_reload_replaces_workers_while_serving() {
    let srv = spawn_with_config("shared/echo-worker.php", 3, "");
    let pids0 = wait_workers(&srv, Duration::from_secs(20), "3 workers", |p| p.len() == 3);
    let storm = storm(srv.addr, 8);
    std::thread::sleep(Duration::from_millis(500));
    signal(srv.pid(), libc::SIGUSR2);
    wait_workers(
        &srv,
        Duration::from_secs(60),
        "reload to 3 disjoint pids",
        |p| p.len() == 3 && p.iter().all(|x| !pids0.contains(x)),
    );
    std::thread::sleep(Duration::from_secs(2));
    let tally = storm.halt();

    assert_eq!(
        tally.refused,
        0,
        "the listener must stay bound across the reload; last error: {:?}\n{}",
        tally.last_err,
        diagnostics(&srv),
    );
    assert!(tally.ok > 0, "storm served no requests during the reload");
    assert_eq!(
        tally.failed,
        0,
        "requests failed beyond tolerated connection drops; last error: {:?}\n{}",
        tally.last_err,
        diagnostics(&srv),
    );
}
