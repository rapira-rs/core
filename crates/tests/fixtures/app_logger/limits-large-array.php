<?php

// Monolog testNormalizeHandleLargeArraysWithExactly1000Items (boundary, untouched) and testNormalizeHandleLargeArrays (over the cap, marked).
\Rapira\log('exactly-1000', \Rapira\LogLevel::Error, ['rows' => range(1, 1000)]);
\Rapira\log('over-cap', \Rapira\LogLevel::Error, ['rows' => range(1, 2000), 'keep' => 'visible']);

echo 'logged';
