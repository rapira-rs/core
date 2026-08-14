<?php
// Forces a TEARDOWN-time zend_bailout. The handler returns normally; then an open
// output-buffer callback fatals during php_output_end_all() *inside*
// rapira_request_teardown() — the exact case the per-call zend_try in that C helper
// must contain. If the guard works, the worker survives and keeps serving.
ini_set('display_errors', '0'); // keep the fatal in the log, out of the response body

class Counter
{
    public static int $n = 0;
}

$handler = static function (): void {
    Counter::$n++;                               // runs BEFORE the bail (proves continuity)
    if (($_GET['boom'] ?? '') === '1') {
        // This callback runs at request teardown (php_output_end_all force-pops the
        // buffer), i.e. AFTER the handler returns — not during handler execution.
        ob_start(static function (string $buf): string {
            trigger_error('boom during output flush', E_USER_ERROR); // E_USER_ERROR -> zend_bailout
            return $buf;                          // unreachable
        });
        echo "never flushes cleanly";            // buffered; never makes it to ub_write
        return;                                   // open buffer is force-flushed at teardown -> bail
    }
    header('Content-Type: text/plain');
    echo "ok counter=" . Counter::$n;
};

while (\Rapira\handle_request($handler)) {
    gc_collect_cycles();
}
