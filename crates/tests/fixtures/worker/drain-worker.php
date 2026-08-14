<?php
// The draining-false contract: the loop exits when the host closes the intake,
// and the script still runs to completion afterwards.
$served = 0;
$handler = static function () use (&$served): void {
    $served++;
    echo 'n=', $served;
};
while (\Rapira\handle_request($handler)) {
}
\Rapira\log('loop-exited served=' . $served);
