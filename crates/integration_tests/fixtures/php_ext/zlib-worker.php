<?php
$handler = static function (): void {
	if (!extension_loaded('zlib')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		gzcompress('x', 100);
		return;
	}
	echo 'zlib:' . gzuncompress(gzcompress('rapira zlib'));
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
