<?php
// Echoes like echo-worker, but ?hang=1 blocks the request forever in native
// sleep — the master's request_terminate_timeout watchdog must kill the worker.
$handler = static function (): void {
    if (($_GET['hang'] ?? '') === '1') {
        while (true) {
            usleep(100000);
        }
    }
    header('Content-Type: text/plain');
    echo 'ok:' . getmypid();
};
while (\rapira_handle_request($handler)) {
}
