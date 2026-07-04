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
	for ($i = 0; $i < 64; $i++) {
		echo str_repeat('x', 32), "\n";
	}
	Track::$done++; // must not run when the client disconnected mid-stream
};
while (\rapira_handle_request($handler)) {
	gc_collect_cycles();
}
