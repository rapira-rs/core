<?php
$handler = static function (): void {
    header('Content-Type: text/plain');
    echo getenv('PATH'), "\n";
};
require __DIR__ . '/nope-does-not-exist.php'; // Fatal: failed opening required file
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) { gc_collect_cycles(); }