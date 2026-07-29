<?php
$handler = static function (): void {
	// Neither of these can go on the wire: a space is not a tchar, and 0x01 is not a
	// legal field-value byte. sapi_header_op screens only CR, LF and NUL, so both
	// reach the SAPI and must be dropped without costing the rest of the response.
	header('Content Type: text/html');
	header("X-Ctl: \x01");
	header('X-Keep: kept');
	http_response_code(201);
	echo 'body';
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
	gc_collect_cycles();
}
