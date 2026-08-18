<?php

// Probes an empty channel: the test never creates a handle, so no job can precede the probes.

use Rapira\Exception\TimeoutException;

$d = \Rapira\get_dispatcher();
$probes = [
    'try' => $d->tryReceive() === null ? 'null' : 'unit',
];
try {
    $d->receive(0);
    $probes['zero'] = 'unit';
} catch (TimeoutException) {
    $probes['zero'] = 'timeout';
}
try {
    $d->receive(50_000);
    $probes['short'] = 'unit';
} catch (TimeoutException) {
    $probes['short'] = 'timeout';
}
\Rapira\log('recv-probes', context: $probes);
