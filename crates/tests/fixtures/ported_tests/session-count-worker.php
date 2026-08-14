<?php
$handler = static function (): void {
	session_start();
	$_SESSION['count'] = isset($_SESSION['count']) ? $_SESSION['count'] + 1 : 0;
	echo "Count: {$_SESSION['count']}\n";
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
