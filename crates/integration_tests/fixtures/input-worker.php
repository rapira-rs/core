<?php
$handler = static function (): void {
	$in = file_get_contents('php://input');
	header('Content-Type: text/plain');
	echo "len=", strlen($in), " body=", $in;
};
while (\rapira_handle_request($handler)) {
	gc_collect_cycles();
}
