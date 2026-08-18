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
	// the test drops the receiver after 'held'; the sleep lets the drop land before the write
	\Rapira\log('held');
	usleep(300000);
	echo "payload\n"; // aborted write: the raised abort must not bail
	TrackIgnore::$reached++; // ignore_user_abort=1: the handler keeps running
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
