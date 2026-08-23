<?php
$handler = static function (): void {
	if (!extension_loaded('Zend OPcache')) {
		echo 'skip';
		return;
	}
	// opcache_get_status() is registered even when accel_startup() failed, and returns false in that case
	$status = opcache_get_status(false);
	echo is_array($status) && ($status['opcache_enabled'] ?? false)
		? 'opcache:enabled'
		: 'opcache:disabled';
};
while (\Rapira\handle_request($handler)) {
}
