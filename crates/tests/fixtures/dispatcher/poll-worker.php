<?php

// Serves through the polling verbs: tryReceive() first, receive(1s) once a
// request flips the mode - the happy path of both non-blocking receives.

$d = \Rapira\get_dispatcher();
$mode = 'try';
while (true) {
    try {
        if ($mode === 'try') {
            $ex = $d->tryReceive();
            if ($ex === null) {
                usleep(1000);
                continue;
            }
        } else {
            $ex = $d->receive(1_000_000);
        }
    } catch (\Rapira\Exception\TimeoutException) {
        continue;
    } catch (\Rapira\Exception\ClosedException) {
        break;
    }
    $req = $ex->getRequest();
    parse_str(parse_url($req->target, PHP_URL_QUERY) ?: '', $q);
    $served = $mode;
    if (($q['mode'] ?? '') !== '') {
        $mode = $q['mode'];
    }
    $ex->writeBody("served-by=$served target={$req->target}");
}
