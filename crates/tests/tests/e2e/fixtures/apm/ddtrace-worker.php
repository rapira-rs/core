<?php
// The tracer's compatible-SAPI gate (dd-trace-php ext/datadog.c; the name strings are
// in components/sapi/sapi.c; docs at
// https://docs.datadoghq.com/tracing/trace_collection/compatibility/php/) has neither
// "fastcgi" (PHP 8.4) nor "rapira" (8.5). MINIT must set datadog_disable and still
// return SUCCESS: the extension loads, keeps its API, and traces nothing. phpinfo()
// renders the flag as the "Datadog tracing support" row. ini_get('ddtrace.disable')
// does not show it: the ini handler writes into the flag one-way.
$handler = static function (): void {
	ob_start();
	phpinfo(INFO_MODULES);
	$info = (string) ob_get_clean();
	// tags and the text-mode " => " both become spaces, so the row parses the same
	// in HTML and text phpinfo output
	$flat = (string) preg_replace('/<[^>]*>/', ' ', $info);
	$flat = trim((string) preg_replace('/\s+/', ' ', str_replace('=>', ' ', $flat)));
	if (str_contains($flat, 'Datadog tracing support disabled (not built)')) {
		$state = 'not-built';
	} elseif (str_contains($flat, 'Datadog tracing support disabled')) {
		$state = 'disabled';
	} elseif (str_contains($flat, 'Datadog tracing support enabled')) {
		$state = 'enabled';
	} else {
		$state = 'unknown';
	}
	$expected = PHP_VERSION_ID >= 80500 ? 'rapira' : 'fastcgi';
	echo implode("\n", [
		'dd:sapi=' . PHP_SAPI,
		'dd:sapi_ok=' . (PHP_SAPI === $expected ? '1' : '0'),
		'dd:version=' . phpversion('ddtrace'),
		'dd:tracing=' . $state,
		'dd:active_span=' . (function_exists('DDTrace\active_span')
			? var_export(\DDTrace\active_span(), true) : 'n/a'),
		'dd:pid=' . getmypid(),
	]);
};
while (\Rapira\handle_request($handler)) {
}
