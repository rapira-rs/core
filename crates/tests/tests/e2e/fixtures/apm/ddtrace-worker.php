<?php
// The tracer's compatible-SAPI list (dd-trace-php ext/datadog.c, docs at
// https://docs.datadoghq.com/tracing/trace_collection/compatibility/php/) has neither
// "fastcgi" (PHP 8.4) nor "rapira" (8.5). MINIT must set datadog_disable and still
// return SUCCESS: the extension loads, keeps its API, and traces nothing. phpinfo()
// renders the flag as the "Datadog tracing support" row. ini_get('ddtrace.disable')
// does not show it: the ini handler writes into the flag one-way.
$handler = static function (): void {
	if (!extension_loaded('ddtrace')) {
		echo 'skip';
		return;
	}
	ob_start();
	phpinfo(INFO_MODULES);
	$info = (string) ob_get_clean();
	// tags become spaces so the phpinfo row reads the same in text and HTML mode
	$flat = trim((string) preg_replace('/\s+/', ' ', (string) preg_replace('/<[^>]*>/', ' ', $info)));
	$state = str_contains($flat, 'Datadog tracing support disabled') ? 'disabled'
		: (str_contains($flat, 'Datadog tracing support enabled') ? 'enabled' : 'unknown');
	echo implode("\n", [
		'dd:sapi=' . PHP_SAPI,
		'dd:version=' . phpversion('ddtrace'),
		'dd:tracing=' . $state,
		'dd:active_span=' . (function_exists('DDTrace\active_span')
			? var_export(\DDTrace\active_span(), true) : 'n/a'),
		'dd:pid=' . getmypid(),
	]);
};
while (\Rapira\handle_request($handler)) {
}
