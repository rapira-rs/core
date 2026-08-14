<?php
// Resident worker: each request raises an E_USER_WARNING carrying a marker.

use Rapira\Exception\ClosedException;

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        trigger_error('WARN-MARK diagnostic', E_USER_WARNING);
        $ex->writeHead(200, ['content-type' => ['text/plain']]);
        $ex->writeBody('ok');
    }
} catch (ClosedException) {
}
