<?php
$handler = static function (): void {
	if (!extension_loaded('dom')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		(new DOMDocument())->createElement('inv alid');
		return;
	}
	$d = new DOMDocument();
	$d->loadXML('<r>ok</r>');
	echo 'dom:' . $d->documentElement->textContent;
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
