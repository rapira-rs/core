<?php
$handler = static function (): void {
	session_start();
	header('Content-Type: text/plain');
	$n = $_SESSION['n'] ?? 0;
	echo "sid=" . session_id() . " n=" . $n;
	$_SESSION['n'] = $n + 1;
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
