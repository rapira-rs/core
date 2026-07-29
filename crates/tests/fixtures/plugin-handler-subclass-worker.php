<?php
use Rapira\Plugin\Http\HttpHandlerConfig;
use function Rapira\create_plugin_handler;

$http = create_plugin_handler(new HttpHandlerConfig());

// PluginHandler is abstract but not final, so this links; zend_do_inheritance copies
// the no-ctor handler into it. The destructor must never run: `new` throws before the
// object is constructed, and $config stays uninitialized.
class Foo extends \Rapira\PluginHandler
{
	public function __destruct()
	{
		echo 'DESTRUCT';
	}
}

$handler = static function (): void {
	header('Content-Type: text/plain');
	try {
		new Foo();
		echo 'no-throw';
	} catch (\Error $e) {
		echo 'threw:', $e->getMessage();
	}
};

while ($http->handleRequest($handler)) {
}
