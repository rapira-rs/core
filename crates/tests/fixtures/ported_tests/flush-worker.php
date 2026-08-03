<?php
$handler = static function (): void {
	echo 'He';
	flush();
	echo 'llo ' . ($_GET['i'] ?? '');
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
