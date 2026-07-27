<?php
$handler = static function (): void {
	file_get_contents('php://input'); // materialize the request body stream
	header('Content-Type: text/plain');
	echo "streams=", count(get_resources('stream'));
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
