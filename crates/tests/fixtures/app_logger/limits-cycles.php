<?php

// Monolog: testIgnoresRecursiveObjectReferences / testCanNormalizeReferences.
// Both cycles must be broken without a diagnostic and without losing siblings.
$foo = new \stdClass();
$bar = new \stdClass();
$foo->bar = $bar;
$bar->foo = $foo;

$x = ['foo' => 'bar'];
$y = ['x' => &$x];
$x['y'] = &$y;

\Rapira\log('cycles', \Rapira\LogLevel::Error, [
    'objects' => $foo,
    'arrays' => $y,
    'keep' => 'visible',
]);

echo 'logged';
