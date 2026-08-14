<?php
$handler = static function (): void {
	if (!extension_loaded('xmlwriter')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		(new XMLWriter())->openUri('');
		return;
	}
	$w = new XMLWriter();
	$w->openMemory();
	$w->writeElement('v', 'ok');
	echo 'xw:' . $w->outputMemory();
};
while (\Rapira\handle_request($handler)) {
}
