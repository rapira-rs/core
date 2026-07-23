<?php
$handler = static function (): void {
	echo "REQUEST:";
	var_export($_REQUEST);
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
