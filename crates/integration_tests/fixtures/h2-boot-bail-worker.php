<?php
// The bootstrap opens a session whose save handler fatals the FIRST time it is
// written. The worker's first-call rapira_request_teardown() flushes that session,
// so that teardown bails. The bootstrap also bumps a persistent counter, so the
// served request reports which cycle it ran in:
//   before the fix: the first-call bailout is swallowed and the job is served in
//     cycle 1, on the un-reclaimed post-longjmp state -> counter "1";
//   after the fix:  the bailout recycles, so the worker re-bootstraps and serves
//     the job in cycle 2 -> counter "2".
// Paths key off getmypid() so the (in-process) test can clean them.
$dir = sys_get_temp_dir();
$sentinel = $dir . '/rapira_h2_sentinel_' . getmypid();
$boot = $dir . '/rapira_h2_boot_' . getmypid();

$n = (file_exists($boot) ? (int) file_get_contents($boot) : 0) + 1;
file_put_contents($boot, (string) $n);

class BoomOnce extends SessionHandler {
    public string $sentinel = '';
    public function write(string $id, string $data): bool {
        if (!file_exists($this->sentinel)) {
            @touch($this->sentinel);
            trigger_error('boot bail', E_USER_ERROR); // bails inside rapira_reset_session
        }
        return true;
    }
}
$save = new BoomOnce();
$save->sentinel = $sentinel;
session_set_save_handler($save);
session_start();
$_SESSION['k'] = 'v'; // dirty the session so the flush actually writes

$handler = static function () use ($boot): void {
    echo (int) file_get_contents($boot);
};
while (\rapira_handle_request($handler)) {
}
