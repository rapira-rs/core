<?php
use Rapira\Plugin\Http\HttpHandlerConfig;
use function Rapira\create_plugin_handler;

$http = create_plugin_handler(new HttpHandlerConfig('/api'));

// Both probes declare a config the factory must refuse. Done from inside the
// handler (worker mode is live either way) so a refusal cannot failboot the
// worker and the test can read the message off the response.
$handler = static function (): void {
	header('Content-Type: text/plain');
	try {
		// A PHP string is a byte string: "\xFF" is not valid UTF-8, so the
		// config cannot be JSON-encoded for the plugin.
		$cfg = ($_GET['probe'] ?? '') === 'utf8'
			? new HttpHandlerConfig("/api\xFF")
			: new HttpHandlerConfig('api');
		create_plugin_handler($cfg);
		echo 'no-throw';
	} catch (\Rapira\RapiraException $e) {
		echo 'threw:', $e->getMessage();
	}
};

while ($http->handleRequest($handler)) {
}
