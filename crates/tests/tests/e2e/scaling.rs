use crate::harness::*;
use std::time::{Duration, Instant};

#[test]
fn dynamic_scales_up_down() {
    // `pool.processes` is the ceiling (max children); dynamic keeps
    // min_spare..max_spare idle.
    let srv = spawn_with_config(
        "sleep-worker.php",
        4,
        "mode = \"dynamic\"\nmin_spare = 1\nmax_spare = 2\n",
    );
    let storm = storm(srv.addr, 4);
    wait_workers(
        &srv,
        Duration::from_secs(30),
        "scale up to >= 2 workers",
        |p| p.len() >= 2,
    );
    let _ = storm.halt();
    wait_workers(
        &srv,
        Duration::from_secs(45),
        "scale down to <= 2 workers",
        |p| p.len() <= 2,
    );
}

#[test]
fn ondemand_spawns_on_connect() {
    let srv = spawn_with_config(
        "echo-worker.php",
        2,
        "mode = \"ondemand\"\nprocess_idle_timeout_secs = 2\n",
    );
    // The master binds the listener pre-fork, so the harness readiness probe
    // connects — which is itself a demand event that forks one worker. Wait for
    // it to appear (the fork may lag the probe), let it idle-retire, then
    // assert the pool sits at zero with no traffic.
    wait_workers(
        &srv,
        Duration::from_secs(20),
        "probe-spawned worker appears",
        |p| !p.is_empty(),
    );
    wait_workers(
        &srv,
        Duration::from_secs(30),
        "probe-spawned worker retires",
        <[u32]>::is_empty,
    );
    let hold = Instant::now() + Duration::from_secs(1);
    while Instant::now() < hold {
        let idle = worker_pids(srv.pid());
        assert!(
            idle.is_empty(),
            "ondemand forked with no pending connection: {idle:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let (code, _) = http_get(srv.addr, "/", Duration::from_secs(10)).expect("GET /");
    assert_eq!(code, 200, "\n{}", diagnostics(&srv));
    wait_workers(
        &srv,
        Duration::from_secs(20),
        "on-demand worker spawns",
        |p| !p.is_empty(),
    );
    wait_workers(
        &srv,
        Duration::from_secs(30),
        "idle worker retires",
        <[u32]>::is_empty,
    );
}
