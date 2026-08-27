<?php
// Each worker takes one RAND_bytes() draw right after the fork and caches it. No two
// workers may return the same draw. The fixture has no skip guard: openssl is in
// every build path, and a missing extension must fail the boot loudly.

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
