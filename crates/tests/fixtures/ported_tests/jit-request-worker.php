<?php
$handler = static function (): void {
	if (($_GET['use_request'] ?? '') === '1') {
		include __DIR__ . '/jit-request-include.php';
	} else {
		echo "SKIPPED";
	}
	echo "\nGET:";
	var_export($_GET);
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
