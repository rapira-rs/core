<?php
// Resident worker: each request raises an E_USER_WARNING carrying a marker.
$handler = static function (): void {
    trigger_error('WARN-MARK diagnostic', E_USER_WARNING);
    header('Content-Type: text/plain');
    echo 'ok';
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
