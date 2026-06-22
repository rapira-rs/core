<?php
$handler = static function (): void {
    header('Content-Type: text/plain');
    echo "Hello from worker, " . ($_GET['name'] ?? 'anonymous') . "!\n";
};
while (\rapira_handle_request($handler)) { gc_collect_cycles(); }