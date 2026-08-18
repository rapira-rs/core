<?php

// precision is a plain scalar ini with a well-known built-in default (14), so a planted php.ini is detectable from the body.

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $ex->writeBody('precision=' . ini_get('precision'));
    }
} catch (\Rapira\Exception\ClosedException) {
}
