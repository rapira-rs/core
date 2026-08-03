<?php
// Reports the parsed upload, so a mangled multipart boundary shows up as 'NO FILE'.
$handler = static function (): void {
    $f = $_FILES['file'] ?? null;
    if ($f === null) {
        echo 'NO FILE';
        return;
    }
    echo $f['name'], '|', $f['error'], '|', file_get_contents($f['tmp_name']), '|', $f['tmp_name'];
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}
