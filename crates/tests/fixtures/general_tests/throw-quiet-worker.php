<?php
$handler = static function (): void {
	ini_set('display_errors', '0'); // no error text in the body: nothing precedes the 500
	throw new \RuntimeException('quiet boom');
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
