<?php
$handler = static function (): void {
    header('Content-Type: text/plain');
    print_r($_SERVER);
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
    gc_collect_cycles();
}
