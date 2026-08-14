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
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
