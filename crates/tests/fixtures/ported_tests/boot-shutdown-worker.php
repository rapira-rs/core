<?php
// A boot-registered shutdown function runs once, at cycle end.
$fired = 0;
register_shutdown_function(static function () use (&$fired): void {
    $fired++;
    \Rapira\log('boot-shutdown fired=' . $fired);
});

$handler = static function () use (&$fired): void {
    echo 'fired=', $fired;
};
while (\Rapira\handle_request($handler)) {
}
