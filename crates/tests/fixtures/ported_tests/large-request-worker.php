<?php
$handler = static function (): void {
	echo 'Request body size: ' . strlen(file_get_contents('php://input'));
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
