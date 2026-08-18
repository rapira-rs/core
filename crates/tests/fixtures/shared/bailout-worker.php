<?php
class Counter { public static int $n = 0; }
$handler = static function (): void {
    Counter::$n++;
    if (($_GET['boom'] ?? '') === '1') {
        exit(1); // zend_bailout with no output -> headers not sent -> 500 path (a string arg prints first -> 200)
    }
    header('Content-Type: text/plain');
    echo "ok counter=" . Counter::$n;
};
while (\Rapira\handle_request($handler)) { gc_collect_cycles(); }