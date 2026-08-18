<?php

// Monolog testMaxNormalizeDepth: at 20000 the encoder runs until Zend's stack guard trips and substitutes a bare null, with nothing marking the cut.
$deep = 'bottom';
for ($i = 0; $i < 20000; $i++) {
    $deep = ['n' => $deep];
}

\Rapira\log('deep', \Rapira\LogLevel::Error, ['tree' => $deep, 'keep' => 'visible']);

echo 'logged';
