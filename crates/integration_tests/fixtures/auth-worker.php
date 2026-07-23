<?php
$handler = static function (): void {
    header('Content-Type: text/plain');
    echo "user=" . ($_SERVER['PHP_AUTH_USER'] ?? '-') . " pass=" . ($_SERVER['PHP_AUTH_PW'] ?? '-');
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
    gc_collect_cycles();
}
