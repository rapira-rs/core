<?php
$handler = static function (): void {
	if (!extension_loaded('openssl')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		openssl_random_pseudo_bytes(0);
		return;
	}
	echo 'openssl:' . strlen(openssl_digest('rapira', 'sha256'));
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
