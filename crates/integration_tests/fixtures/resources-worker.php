<?php
$handler = static function (): void {
	file_get_contents('php://input'); // materialize the request body stream
	header('Content-Type: text/plain');
	echo "streams=", count(get_resources('stream'));
};
while (\rapira_handle_request($handler)) {
	gc_collect_cycles();
}
