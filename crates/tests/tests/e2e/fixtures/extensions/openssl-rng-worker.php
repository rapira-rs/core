<?php
// Each worker takes one RAND_bytes() draw right after the fork and caches it. OpenSSL
// reseeds its DRBG when the fork id changes, so no two workers may return the same draw.

use Rapira\Exception\ClosedException;

$first = bin2hex(openssl_random_pseudo_bytes(16));
$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $ex->writeBody('pid=' . getmypid() . ' first=' . $first);
    }
} catch (ClosedException) {
}
