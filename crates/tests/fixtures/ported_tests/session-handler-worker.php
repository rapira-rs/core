<?php
class ArrayHandler implements \SessionHandlerInterface
{
	private static array $data = [];

	public function open(string $path, string $name): bool
	{
		return true;
	}

	public function close(): bool
	{
		return true;
	}

	public function read(string $id): string|false
	{
		return self::$data[$id] ?? '';
	}

	public function write(string $id, string $data): bool
	{
		self::$data[$id] = $data;
		return true;
	}

	public function destroy(string $id): bool
	{
		unset(self::$data[$id]);
		return true;
	}

	public function gc(int $max_lifetime): int|false
	{
		return 0;
	}
}

$handler = static function (): void {
	if (($_GET['action'] ?? '') === 'register') {
		session_set_save_handler(new ArrayHandler(), true);
		session_id('fixed-id-1');
		session_start();
		$_SESSION['value'] = 'v1';
		session_write_close();
		echo "REGISTERED save_handler=", ini_get('session.save_handler');
		return;
	}
	// no registration here: request 1's handler must still be installed
	$err = '';
	set_error_handler(static function (int $no, string $msg) use (&$err): bool {
		$err .= $msg;
		return true;
	});
	try {
		session_id('fixed-id-2');
		$ok = session_start();
		if ($ok) {
			session_write_close();
		}
		echo "START=", $ok ? 'true' : 'false';
	} catch (\Throwable $e) {
		echo "EXCEPTION:", $e->getMessage();
	}
	restore_error_handler();
	if ($err !== '') {
		echo " ERROR:", $err;
	}
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
