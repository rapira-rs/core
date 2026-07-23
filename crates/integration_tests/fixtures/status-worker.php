<?php
$handler = static function (): void {
	if (($_GET['code'] ?? '') === '404') {
		http_response_code(404);
	}
	header('Content-Type: text/plain');
	echo "ok";
};

$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
