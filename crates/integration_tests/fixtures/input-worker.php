<?php
$handler = static function (): void {
	$in = file_get_contents('php://input');
	header('Content-Type: text/plain');
	echo "len=", strlen($in), " body=", $in;
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
