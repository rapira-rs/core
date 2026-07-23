<?php
set_exception_handler(static function (\Throwable $e): void {
	header('Content-Type: text/plain');
	echo "handled:", $e->getMessage();
});
$handler = static function (): void {
	throw new \RuntimeException('boom');
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
