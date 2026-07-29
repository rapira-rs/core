<?php
$handler = static function (): void {
	if (!extension_loaded('xml')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		xml_parser_set_option(xml_parser_create(), 424242, 1);
		return;
	}
	echo 'xml:' . xml_parse(xml_parser_create(), '<a>b</a>', true);
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
