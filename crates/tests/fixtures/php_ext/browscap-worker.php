<?php
$handler = static function (): void {
	// the unset-path assertions need no system browscap; skip when one is configured
	if ((string) ini_get('browscap') !== '') {
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
