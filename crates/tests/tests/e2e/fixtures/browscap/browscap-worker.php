<?php
// get_browser() with a null name reads $_SERVER['HTTP_USER_AGENT'], so this fixture needs worker mode.
$handler = static function (): void {
	// ?probe=pid uses an explicit user agent and appends the pid, for the shared-table test
	$probe = ($_GET['probe'] ?? '') === 'pid';
	$b = get_browser($probe ? 'Rapira/1.0 (Darwin)' : null, true);
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
	if ($probe) {
		echo "\npid=" . getmypid();
	}
};
while (\Rapira\handle_request($handler)) {
}
