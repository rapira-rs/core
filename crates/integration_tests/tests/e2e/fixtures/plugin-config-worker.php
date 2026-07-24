<?php
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig(pathPrefix: '/api'));
while ($http->handleRequest(static function (): void {
	header('Content-Type: text/plain');
	echo 'served:' . $_SERVER['REQUEST_URI'];
})) {
}
