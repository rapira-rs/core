<?php
$handler = static function (): void {
	session_start();
	header('Content-Type: text/plain');
	$n = $_SESSION['n'] ?? 0;
	echo "sid=" . session_id() . " n=" . $n;
	$_SESSION['n'] = $n + 1;
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
