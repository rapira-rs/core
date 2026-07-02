<?php
$handler = static function (): void {
	if (($_GET['code'] ?? '') === '404') {
		http_response_code(404);
	}
	header('Content-Type: text/plain');
	echo "ok";
};
while (\rapira_handle_request($handler)) {
	gc_collect_cycles();
}
