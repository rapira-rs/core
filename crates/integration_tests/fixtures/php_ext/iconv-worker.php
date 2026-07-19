<?php
$handler = static function (): void {
	if (!extension_loaded('iconv')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		// a bogus encoding only warns; a non-string arg throws
		iconv('UTF-8', 'UTF-8', []);
		return;
	}
	echo 'iconv:' . iconv('UTF-8', 'UTF-8', 'iconv ok');
};
while (\rapira_handle_request($handler)) {
}
