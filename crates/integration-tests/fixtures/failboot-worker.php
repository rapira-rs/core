<?php
$handler = static function (): void {
    header('Content-Type: text/plain');
    echo getenv('PATH'), "\n";
};
require __DIR__ . '/nope-does-not-exist.php'; // Fatal: failed opening required file
while (\rapira_handle_request($handler)) { gc_collect_cycles(); }