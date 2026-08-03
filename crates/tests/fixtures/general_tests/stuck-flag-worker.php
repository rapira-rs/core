<?php
class BailWrapper
{
	public $context;
	public function stream_open($p, $m, $o, &$op): bool
	{
		trigger_error('boom', E_USER_ERROR); // guaranteed bailout, no memory dependency
		return false;
	}
}
stream_wrapper_register('bail', BailWrapper::class); // is_url=0 local wrapper
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest(static function (): void {
	if (($_GET['step'] ?? '') === 'boom') {
		include 'bail://x'; // sets PG(in_user_include)=1, then bails -> restore (userspace.c:371) skipped
		return;
	}
	// data:// is is_url=1 -> rejected iff in_user_include is stranded (allow_url_include=0 default)
	echo file_get_contents('data://text/plain,ok') === 'ok' ? 'PROBE_OK' : 'PROBE_REJECTED';
}));
