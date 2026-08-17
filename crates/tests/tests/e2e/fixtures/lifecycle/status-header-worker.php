<?php
// `Status: NNN` is the CGI idiom for setting the response code. sapi_header_op gives it no
// special handling, so the origin server has to consume it - otherwise it goes out as a
// literal field on a 200.
$handler = static function (): void {
    header('Status: 404 Not Found');
    header('X-Keep: kept');
    echo 'body';
};
while (\Rapira\handle_request($handler)) {
}
