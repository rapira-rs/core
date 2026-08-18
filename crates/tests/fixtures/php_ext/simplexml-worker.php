<?php
$handler = static function (): void {
	if (!extension_loaded('simplexml')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		libxml_use_internal_errors(true);
		new SimpleXMLElement('not xml at all');
		return;
	}
	echo 'sxml:' . (new SimpleXMLElement('<r><v>ok</v></r>'))->v;
};
while (\Rapira\handle_request($handler)) {
}
