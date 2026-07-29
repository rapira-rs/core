<?php
class BombHandler extends \SessionHandler
{
	public function write(string $id, string $data): bool
	{
		trigger_error('write bomb', E_USER_ERROR); // bails inside the teardown flush
	}
}
session_set_save_handler(new BombHandler(), false);
$handler = static function (): void {
	session_start();
	header('Content-Type: text/plain');
	echo "sid=", session_id();
	$_SESSION['n'] = 1; // dirty session -> teardown flush -> write() -> bailout
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
