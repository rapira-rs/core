<?php
$served = 0;
$handler = static function () use (&$served): void {
	$served++;
	if ($served === 1) {
		$d = new class {
			public function __destruct()
			{
				throw new \RuntimeException('dtor boom');
			}
		};
		// the closure keeps $d alive until the shutdown table is freed after
		// the job; the destructor then throws outside any handler frame
		register_shutdown_function(static function () use ($d): void {});
	}
	echo 'served=', $served;
};
while (\Rapira\handle_request($handler)) {
}
