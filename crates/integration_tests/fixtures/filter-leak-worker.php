<?php
$handler = static function (): void {
    if (!extension_loaded('filter')) {
        echo 'skip';
        return;
    }
    echo 'mem=', memory_get_usage(false);
};

$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}