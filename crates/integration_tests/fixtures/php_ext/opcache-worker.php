<?php
$handler = static function (): void {
	if (!extension_loaded('Zend OPcache')) {
		echo 'skip';
		return;
	}
	// opcache_get_status() is registered whether or not accel_startup() succeeded,
	// and returns false when it did not — so this distinguishes the two states.
	$status = opcache_get_status(false);
	echo is_array($status) && ($status['opcache_enabled'] ?? false)
		? 'opcache:enabled'
		: 'opcache:disabled';
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
