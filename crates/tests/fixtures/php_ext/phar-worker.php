<?php
$handler = static function (): void {
	if (!extension_loaded('phar')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		new Phar('/nonexistent-dir-xyz/foo.phar');
		return;
	}
	echo 'phar:' . Phar::apiVersion();
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
