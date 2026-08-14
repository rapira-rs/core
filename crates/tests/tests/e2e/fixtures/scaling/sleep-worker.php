<?php
// Same as echo-worker, but each request stays ACTIVE for 300ms so the pool
// accrues busy workers under load (scaling tests).

use Rapira\Exception\ClosedException;

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        usleep(300000);
        $ex->writeHead(200, ['content-type' => ['text/plain']]);
        $ex->writeBody('ok:' . getmypid());
    }
} catch (ClosedException) {
}
