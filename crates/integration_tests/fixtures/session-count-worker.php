<?php
$handler = static function (): void {
	session_start();
	$_SESSION['count'] = isset($_SESSION['count']) ? $_SESSION['count'] + 1 : 0;
	echo "Count: {$_SESSION['count']}\n";
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
