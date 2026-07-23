<?php
$handler = static function (): void {
    header('Content-Type: text/plain');
    echo getenv('FOO'), "\n";
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
    gc_collect_cycles();
}
