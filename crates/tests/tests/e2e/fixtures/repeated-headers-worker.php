<?php
// Echoes what repeated request fields look like to PHP once the front has combined them.
$handler = static function (): void {
    header('Content-Type: text/plain');
    echo ($_COOKIE['a'] ?? '-'), ',', ($_COOKIE['b'] ?? '-'), "\n";
    echo $_SERVER['HTTP_COOKIE'] ?? '-', "\n";
    echo $_SERVER['HTTP_X_FORWARDED_FOR'] ?? '-', "\n";
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
