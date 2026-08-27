<?php
// The agent takes the web path under rapira: its SAPI name is not "cli". The agent
// must load, register its API, and leave the request cycle unchanged with no daemon
// and no valid license. The Rust side spawns this fixture only when newrelic.so
// exists, so a load failure surfaces as a failed assertion, not a skip.
$handler = static function (): void {
	if (($_GET['boom'] ?? '') === '1') {
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
