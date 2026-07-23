<?php
echo "WORKER-BOOT-OUTPUT";          // emitted before the loop -> ub_write with no ctx
while (ob_get_level() > 0) {
	ob_end_flush();                 // force it out to ub_write here (not buffered to later)
}
$handler = static function (): void {
	header('Content-Type: text/plain');
	echo "served";
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
