<?php

// Values json_encode cannot represent. A logger is called from catch blocks, so
// none of these may throw or lose the record: PHP must keep running to the echo.
$fh = fopen('php://memory', 'rb');

\Rapira\log('hostile', \Rapira\LogLevel::Error, [
    'keep' => 'visible',
    'closure' => static fn (): int => 1,
    'resource' => $fh,
    'nan' => NAN,
    'inf' => INF,
    // Raw bytes off the wire: invalid UTF-8 is the usual json_encode failure.
    'bytes' => "\xC3\x28\xA0\xA1",
    // A pure enum is not JsonSerializable and has no backing value.
    'pure_enum' => \Rapira\LogLevel::Debug,
]);

fclose($fh);
echo 'logged';
