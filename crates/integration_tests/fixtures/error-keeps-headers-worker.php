<?php
$handler = static function (): void {
	ini_set('display_errors', '0');
	session_start();
	http_response_code(404);
	throw new \RuntimeException('boom');
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
