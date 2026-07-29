<?php
// Resident worker: each request echoes the serving pid, a continuity probe.
$handler = static function (): void {
    header('Content-Type: text/plain');
    echo 'ok:' . getmypid();
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
