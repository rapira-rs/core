<?php
$handler = static function (): void {
    header('Content-Type: text/plain');
    if (($_GET['mode'] ?? '') === 'spin') {
        // Busy loop: the Zend VM checks vm_interrupt on loop back-edges, so the
        // execution timer's SIGRTMIN turns this into a "Maximum execution time
        // exceeded" fatal.
        while (true) {
        }
    }
    echo 'ok';
};
while (\Rapira\handle_request($handler)) {
}
