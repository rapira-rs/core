<?php
class DtorProbe
{
	public static int $count = 0;

	public function __destruct()
	{
		self::$count++;
	}
}

$path = tempnam(sys_get_temp_dir(), 'rapira_dtor_');
$file = new SplFileObject($path, 'w');
$probe = new DtorProbe();
$handler = static function () use ($file, $probe): void {
	try {
		$file->fwrite('x');
		echo 'write=ok';
	} catch (\Throwable $e) {
		echo 'write=', $e->getMessage();
	}
	echo ' dtors=', DtorProbe::$count;
	echo ' id=', spl_object_id(new stdClass());
};
while (\Rapira\handle_request($handler)) {
}
@unlink($path);
