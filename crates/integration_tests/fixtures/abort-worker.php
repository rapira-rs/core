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
	// The test drops the response receiver right after submitting; the sleep
	// guarantees the drop has landed before the first write observes it.
	usleep(300000);
	echo "payload\n"; // aborted write: the SAPI raises php_handle_aborted_connection
	Track::$done++; // must not run when the client disconnected
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
