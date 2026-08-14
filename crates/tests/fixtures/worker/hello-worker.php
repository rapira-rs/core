<?php
$handler = static function (): void {
    header('content-type: text/plain');
    echo 'hello:', $_SERVER['REQUEST_METHOD'] ?? '?', ':', $_GET['q'] ?? '-';
};
while (\Rapira\handle_request($handler)) {
}
