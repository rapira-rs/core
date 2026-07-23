<?php
$handler = static function (): void {
	if (!extension_loaded('xmlreader')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		XMLReader::XML('');
		return;
	}
	$r = XMLReader::XML('<a>ok</a>');
	$r->read();
	echo 'xr:' . $r->name;
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
