<?php
$handler = static function (): void {
	if (!extension_loaded('curl')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		curl_setopt(curl_init(), -12345, true);
		return;
	}
	echo 'curl:' . curl_version()['version'];
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
