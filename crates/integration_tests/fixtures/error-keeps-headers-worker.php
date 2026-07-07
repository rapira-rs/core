<?php
$handler = static function (): void {
	ini_set('display_errors', '0');
	session_start();
	http_response_code(404);
	throw new \RuntimeException('boom');
};
while (\rapira_handle_request($handler)) {
}
