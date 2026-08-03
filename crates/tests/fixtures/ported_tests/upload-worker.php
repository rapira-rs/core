<?php
$handler = static function (): void {
	$f = $_FILES['file'] ?? null;
	if ($f === null) {
		echo 'NO FILE';
		return;
	}
	echo $f['name'], '|', $f['error'], '|', file_get_contents($f['tmp_name']), '|', $f['tmp_name'];
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
