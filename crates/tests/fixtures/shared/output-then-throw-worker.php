<?php
$handler = static function (): void {
	echo 'hello ';
	throw new \Exception('request ' . ($_GET['i'] ?? '?'));
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
