<?php
$handler = static function (): void {
	if (!extension_loaded('mbstring')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		mb_convert_encoding('x', 'BOGUS-ENC');
		return;
	}
	echo 'mb:' . mb_strtoupper('héllo');
};
while (\rapira_handle_request($handler)) {
}
