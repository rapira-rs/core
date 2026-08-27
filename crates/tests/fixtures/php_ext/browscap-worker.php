<?php
$handler = static function (): void {
	if (!function_exists('get_browser')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		get_browser([]);
		return;
	}
	echo 'browscap:' . var_export(get_browser('Mozilla/5.0'), true);
};
while (\Rapira\handle_request($handler)) {
}
