<?php
$handler = static function (): void {
	ini_set('display_errors', '0');
	session_start();
	http_response_code(404);
	throw new \RuntimeException('boom');
};
while (\Rapira\handle_request($handler)) {
}
