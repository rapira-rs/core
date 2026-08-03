<?php
// The engine fills PG(last_error_*) whatever the mask is, so the teardown log is the only
// place error_reporting() can still be honoured. Set at bootstrap: worker mode restores INI
// per cycle, not per job, so the mask holds for every request below.
error_reporting(E_ALL & ~E_DEPRECATED & ~E_USER_DEPRECATED);

$handler = static function (): void {
	switch ($_GET['step'] ?? '') {
		case 'deprecated':
			trigger_error('MASKED-DEPRECATION', E_USER_DEPRECATED);
			break;
		case 'warn':
			trigger_error('REPORTED-WARNING', E_USER_WARNING);
			break;
		case 'boom':
			// an uncaught throw reaches php_error_cb as E_ERROR without bailing out;
			// trigger_error(E_USER_ERROR) would emit its own deprecation on 8.4+
			throw new \RuntimeException('REPORTED-FATAL');
		case 'silent-fatal':
			error_reporting(0);
			throw new \RuntimeException('SILENCED-FATAL');
		case 'logged':
			// unmasked, and with log_errors the SAPI log callback reports it as well
			error_reporting(E_ALL);
			ini_set('log_errors', '1');
			trigger_error('LOGGED-DEPRECATION', E_USER_DEPRECATED);
			break;
	}
	echo 'ok';
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
