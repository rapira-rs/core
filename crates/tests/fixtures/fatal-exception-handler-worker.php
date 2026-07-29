<?php
set_exception_handler(static function (\Throwable $e): void {
	trigger_error('handler bomb', E_USER_ERROR);
});
$handler = static function (): void {
	throw new \RuntimeException('boom');
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
