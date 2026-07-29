<?php
use Rapira\Plugin\Http\HttpHandlerConfig;
use function Rapira\create_plugin_handler;

$http = create_plugin_handler(new HttpHandlerConfig());

// A config class no plugin claims. Declared from inside the handler so a refusal
// cannot failboot the worker and the test can read the message off the response.
readonly class UnknownConfig extends \Rapira\PluginHandlerConfig {}

$handler = static function (): void {
	header('Content-Type: text/plain');
	try {
		create_plugin_handler(new UnknownConfig());
		echo 'no-throw';
	} catch (\Rapira\RapiraException $e) {
		echo 'threw:', $e->getMessage();
	}
};

while ($http->handleRequest($handler)) {
}
