<?php
$handler = static function (): void {
	if (!extension_loaded('pdo_sqlite')) {
		echo 'skip';
		return;
	}
	if (($_GET['boom'] ?? '') === '1') {
		ini_set('display_errors', '0');
		(new PDO('sqlite::memory:'))->query('DEFINITELY NOT SQL');
		return;
	}
	$db = new PDO('sqlite::memory:');
	$db->exec('CREATE TABLE t (v TEXT)');
	$db->exec("INSERT INTO t VALUES ('ok')");
	echo 'pdo:' . $db->query('SELECT v FROM t')->fetchColumn();
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
