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
while (\rapira_handle_request($handler)) {
}
