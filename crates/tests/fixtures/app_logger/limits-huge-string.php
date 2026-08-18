<?php

// NormalizerFormatter caps item count and depth, never string length: this one 5 MiB scalar produces a 5.24 MB record.
\Rapira\log('huge-string', \Rapira\LogLevel::Error, [
    'blob' => str_repeat('A', 5 * 1024 * 1024),
    'keep' => 'visible',
]);

echo 'logged';
