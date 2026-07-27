<?php
// display_errors=Off keeps fatal text out of the stream; the observable
// outcome is the 500 head committed by the recycle path
ini_set('display_errors', '0');
class Counter { public static int $n = 0; }
$handler = static function (): void {
    Counter::$n++;
    if (($_GET['boom'] ?? '') === '1') {
        ob_start(static function ($buf, $phase) {
            trigger_error('fatal in flush', E_USER_ERROR); // bails out of php_output_end_all
        });
        echo 'buffered';
        \rapira_finish_request();   // the flush runs the ob handler -> fatal
        echo 'resumed-after-fatal'; // must never run: the bailout ends the job
        return;
    }
    header('Content-Type: text/plain');
    echo 'ok counter=' . Counter::$n;
};
$http = Rapira\create_plugin_handler(new Rapira\Plugin\Http\HttpHandlerConfig());
while ($http->handleRequest($handler)) {
}