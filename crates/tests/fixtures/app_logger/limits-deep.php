<?php

// Monolog: testMaxNormalizeDepth. 20000 is far past any usable depth; measured,
// the encoder runs until Zend's stack guard trips and substitutes a bare null
// with nothing to say a cut happened.
$deep = 'bottom';
for ($i = 0; $i < 20000; $i++) {
    $deep = ['n' => $deep];
}

\Rapira\log('deep', \Rapira\LogLevel::Error, ['tree' => $deep, 'keep' => 'visible']);

echo 'logged';
