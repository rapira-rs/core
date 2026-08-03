<?php
$handler = static function (): void {
	if (($_GET['boom'] ?? '') === '1') {
		throw new \RuntimeException("scoreboard error");
	}
	echo "ok";
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
