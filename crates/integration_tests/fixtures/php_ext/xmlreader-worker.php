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
while (\rapira_handle_request($handler)) {
}
