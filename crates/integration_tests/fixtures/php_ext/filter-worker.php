<?php
$handler = static function (): void {
	if (!extension_loaded('filter')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		// an unknown filter id only warns; a non-int filter throws
		filter_var('x', []);
		return;
	}
	echo 'filter:' . filter_var('a@b.com', FILTER_VALIDATE_EMAIL);
};
while (\rapira_handle_request($handler)) {
}
