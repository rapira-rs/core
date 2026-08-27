<?php
// The agent's only SAPI-name test is `name == "cli"`, so rapira takes the web path.
// The agent must load, register its API, and leave the request cycle unchanged with
// no daemon and no valid license.
$handler = static function (): void {
	if (!extension_loaded('newrelic')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		throw new RuntimeException('newrelic-worker: uncaught');
	}
	$api = ['newrelic_start_transaction', 'newrelic_end_transaction', 'newrelic_name_transaction',
		'newrelic_add_custom_parameter', 'newrelic_notice_error', 'newrelic_record_custom_event'];
	foreach ($api as $f) {
		if (!function_exists($f)) {
			echo 'nr:missing:' . $f;
			return;
		}
	}
	newrelic_name_transaction('rapira-e2e');
	newrelic_add_custom_parameter('rapira', 'e2e');
	newrelic_notice_error('rapira e2e probe');
	echo 'nr:ok:' . phpversion('newrelic') . ':' . getmypid();
};
while (\Rapira\handle_request($handler)) {
}
