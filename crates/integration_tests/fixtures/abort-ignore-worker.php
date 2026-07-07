<?php
class TrackIgnore
{
	public static int $reached = 0;
}
$handler = static function (): void {
	header('Content-Type: text/plain');
	if (($_GET['probe'] ?? '') === '1') {
		echo "reached=", TrackIgnore::$reached, " aborted=", connection_aborted();
		return;
	}
	ignore_user_abort(true);
	for ($i = 0; $i < 64; $i++) {
		echo str_repeat('x', 32), "\n";
	}
	TrackIgnore::$reached++; // ignore_user_abort=1: the aborted write must NOT bail
};
while (\rapira_handle_request($handler)) {
	gc_collect_cycles();
}
