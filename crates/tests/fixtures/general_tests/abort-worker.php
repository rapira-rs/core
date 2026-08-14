<?php
class Track
{
	public static int $done = 0;
}

$handler = static function (): void {
	header('Content-Type: text/plain');
	if (($_GET['probe'] ?? '') === '1') {
		echo "done=", Track::$done, " aborted=", connection_aborted();
		return;
	}
	// The test waits for the 'held' record, then drops the receiver; the
	// sleep guarantees the drop has landed before the first write observes it.
	\Rapira\log('held');
	usleep(300000);
	echo "payload\n"; // aborted write: the SAPI raises php_handle_aborted_connection
	Track::$done++; // must not run when the client disconnected
};
while (\Rapira\handle_request($handler)) {
	gc_collect_cycles();
}
