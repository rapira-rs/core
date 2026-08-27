<?php
// get_browser() with a null name reads $_SERVER['HTTP_USER_AGENT']. This fixture
// therefore needs worker mode and also covers the SAPI's register_server_variables.
$handler = static function (): void {
	$b = get_browser(null, true);
	if (!is_array($b)) {
		echo 'browscap:false';
		return;
	}
	$keys = ['browser', 'platform', 'crawler', 'version', 'parent', 'browser_name_pattern', 'browser_name_regex'];
	$out = [];
	foreach ($keys as $k) {
		$out[] = $k . '=' . (array_key_exists($k, $b) ? $b[$k] : '<unset>');
	}
	echo implode("\n", $out);
};
while (\Rapira\handle_request($handler)) {
}
