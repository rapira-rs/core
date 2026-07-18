<?php
class BoomHandler extends SessionHandler
{
	public function write(string $id, string $data): bool
	{
		trigger_error('boom', E_USER_ERROR);
	}
}
$handler = static function (): void {
	if (($_GET['probe'] ?? '') === '1') {
		echo gc_status()['protected'] ? 'protected' : 'unprotected';
		return;
	}
	session_set_save_handler(new BoomHandler());
	session_start();
	$_SESSION['k'] = 'v';
	echo 'seeded';
};
while (\rapira_handle_request($handler)) {
}
