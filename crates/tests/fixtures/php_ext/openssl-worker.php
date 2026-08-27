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
	// ext/openssl has no RINIT or RSHUTDOWN. The error ring lives in persistent
	// memory (php_openssl.h). ?step=leak leaves one error for the next request.
	if (($_GET['step'] ?? '') === 'leak') {
		@openssl_x509_read('not a certificate');
		echo 'openssl:leaked';
		return;
	}
	if (($_GET['step'] ?? '') === 'drain') {
		$errs = [];
		while (($e = openssl_error_string()) !== false) {
			$errs[] = $e;
		}
		echo 'openssl:drained:' . count($errs) . ':' . implode('|', $errs);
		return;
	}
	echo 'openssl:' . strlen(openssl_digest('rapira', 'sha256'));
};
while (\Rapira\handle_request($handler)) {
}
