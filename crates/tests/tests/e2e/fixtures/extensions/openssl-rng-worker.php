<?php
// Each worker caches one RAND_bytes() draw taken right after the fork; no two workers may return the same draw.

use Rapira\Exception\ClosedException;

// no skip guard: openssl is in every build, and a missing extension must fail the boot loudly
$first = bin2hex(openssl_random_pseudo_bytes(16));
$d = \Rapira\get_dispatcher();
try {
	while (true) {
		$ex = $d->receive();
		$ex->writeBody('pid=' . getmypid() . ' first=' . $first);
	}
} catch (ClosedException) {
}
