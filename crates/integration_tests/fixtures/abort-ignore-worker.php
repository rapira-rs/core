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
	// The test drops the response receiver right after submitting; the sleep
	// guarantees the drop has landed before the write observes it.
	usleep(300000);
	echo "payload\n"; // aborted write: the raised abort must NOT bail
	TrackIgnore::$reached++; // ignore_user_abort=1: the handler keeps running
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
