<?php
function helper(string $big): void
{
	trigger_error('boom', E_USER_ERROR); // $big captured by ref in EG(last_fatal_error_backtrace)
}
$handler = static function (): void {
	set_error_handler(static fn(): bool => true); // consume -> execution continues, helper's frame unwinds
	if (($_GET['step'] ?? '') === 'boom') {
		helper(str_repeat('x', 20 * 1024 * 1024)); // after helper returns, 20MB pinned ONLY by the backtrace
		echo 'boomed';
		return;
	}
	echo 'mem=' . memory_get_usage(false);
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
