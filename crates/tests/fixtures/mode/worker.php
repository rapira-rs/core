<?php
// The boot answer is captured once, then every job re-asks: the case must stay identical.
$boot = \Rapira\get_mode();
$handler = static function () use ($boot): void {
    $mode = \Rapira\get_mode();
    echo $mode->name,
        ':', $mode === \Rapira\Mode::Worker ? 'case' : 'copy',
        ':', $mode === $boot ? 'same' : 'new',
        ':', $mode instanceof \BackedEnum ? 'backed' : 'unbacked';
};
while (\Rapira\handle_request($handler)) {
}
