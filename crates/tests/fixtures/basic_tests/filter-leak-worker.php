<?php
$handler = static function (): void {
    if (!extension_loaded('filter')) {
        echo 'skip';
        return;
    }
    echo 'mem=', memory_get_usage(false);
};

while (\Rapira\handle_request($handler)) {
}