<?php
$handler = static function (): void {
	var_export($_COOKIE);
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
