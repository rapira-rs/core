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
while (\rapira_handle_request($handler)) {
}
