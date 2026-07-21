<?php
// Resident worker: each request echoes the serving pid, a continuity probe.
$handler = static function (): void {
    header('Content-Type: text/plain');
    echo 'ok:' . getmypid();
};
while (\rapira_handle_request($handler)) {
}
