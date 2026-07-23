<?php
// Same as echo-worker, but each request stays ACTIVE for 300ms so the pool
// accrues busy workers under load (scaling tests).
$handler = static function (): void {
    header('Content-Type: text/plain');
    usleep(300000);
    echo 'ok:' . getmypid();
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
