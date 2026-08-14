<?php
$handler = static function (): void {
    header('Content-Type: text/plain');
    print_r($_SERVER);
};
while (\Rapira\handle_request($handler)) {
    gc_collect_cycles();
}
