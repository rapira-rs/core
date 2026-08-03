<?php

// Monolog testFormat: INF => "INF", -INF => "-INF", NAN => "NaN".
// Monolog testIgnoresInvalidEncoding: bad bytes become U+FFFD, the value survives.
\Rapira\log('scalars', \Rapira\LogLevel::Error, [
    'inf' => INF,
    'ninf' => -INF,
    'nan' => acos(4),
    // "\xB1\x31" is Monolog's own fixture: an invalid lead byte followed by "1".
    'bad_utf8' => "\xB1\x31",
    'keep' => 'visible',
]);

echo 'logged';
