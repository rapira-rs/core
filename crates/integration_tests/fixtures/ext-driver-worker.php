<?php
// Resident worker script: handles each request an extension drives via `exec`.
$handler = static function (): void {
	header('Content-Type: text/plain');
	echo 'ok:' . ($_GET['from'] ?? '?');
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
