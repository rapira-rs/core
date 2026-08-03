<?php
class Counter
{
    public static int $n = 0;
}
$handler = static function (): void {
    Counter::$n++;
    register_shutdown_function(static fn() => error_log("[shutdown] ran")); // STDERR is undefined in embed SAPI
    header('Content-Type: text/plain');
    echo "counter=" . Counter::$n . " session=" . (isset($_SESSION['seen']) ? 'leaked' : 'clean');
    $_SESSION['seen'] = true;
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
    gc_collect_cycles();
}
