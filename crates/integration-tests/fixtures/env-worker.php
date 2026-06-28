<?php
$handler = static function (): void {
    header('Content-Type: text/plain');
    echo getenv('FOO'), "\n";
};
while (\rapira_handle_request($handler)) { gc_collect_cycles(); }