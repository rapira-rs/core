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
while (\Rapira\handle_request($handler)) {
}
