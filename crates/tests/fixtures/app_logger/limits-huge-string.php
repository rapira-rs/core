<?php

// Beyond Monolog: NormalizerFormatter caps item count and depth, never string
// length. Measured, one 5 MiB scalar produces a 5.24 MB log record.
\Rapira\log('huge-string', \Rapira\LogLevel::Error, [
    'blob' => str_repeat('A', 5 * 1024 * 1024),
    'keep' => 'visible',
]);

echo 'logged';
