<?php
$handler = static function (): void {
    header('Content-Type: text/plain');
    echo "user=" . ($_SERVER['PHP_AUTH_USER'] ?? '-') . " pass=" . ($_SERVER['PHP_AUTH_PW'] ?? '-');
};
while (\rapira_handle_request($handler)) { gc_collect_cycles(); }