<?php
// A space is not a tchar and 0x01 is not a legal field-value byte, but sapi_header_op screens only CR, LF and NUL, so both reach the SAPI and must be dropped without costing the rest of the response.
$handler = static function (): void {
    header('Content Type: text/html');
    header("X-Ctl: \x01");
    header('X-Keep: kept');
    http_response_code(201);
    echo 'body';
};
while (\Rapira\handle_request($handler)) {
}
