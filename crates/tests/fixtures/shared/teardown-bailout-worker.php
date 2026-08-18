<?php
// Teardown-time zend_bailout: the handler returns normally, then an open output-buffer callback fatals during php_output_end_all() inside rapira_request_teardown(), which the per-call zend_try in that C helper must contain.
ini_set('display_errors', '0'); // keep the fatal in the log, out of the response body

class Counter
{
    public static int $n = 0;
}

$handler = static function (): void {
    Counter::$n++;
    if (($_GET['boom'] ?? '') === '1') {
        // the callback runs at teardown, when php_output_end_all force-pops the buffer, not during the handler
        ob_start(static function (string $buf): string {
            trigger_error('boom during output flush', E_USER_ERROR); // E_USER_ERROR -> zend_bailout
            return $buf;
        });
        echo "never flushes cleanly";            // buffered; never makes it to ub_write
        return;
    }
    header('Content-Type: text/plain');
    echo "ok counter=" . Counter::$n;
};

while (\Rapira\handle_request($handler)) {
    gc_collect_cycles();
}
