<?php
// Booted once per worker; the handler runs for every request.
$booted = date(DATE_ATOM); // resident: computed once, survives across requests

$handler = static function () use ($booted): void {
    header('content-type: text/plain');
    echo 'worker: ', $_SERVER['REQUEST_METHOD'], ' ', $_SERVER['REQUEST_URI'], "\n";
    echo 'booted: ', $booted, "\n";
};

while (\Rapira\handle_request($handler)) {
}
