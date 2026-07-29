<?php
class Counter
{
	public static int $n = 0;
}
$handler = static function (): void {
	Counter::$n++;
	if (($_GET['boom'] ?? '') === '1') {
		register_shutdown_function(static function (): void {
			trigger_error('shutdown bomb', E_USER_ERROR); // absorbed by php_call_shutdown_functions' zend_try
		});
	}
	header('Content-Type: text/plain');
	echo 'ok counter=' . Counter::$n;
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
