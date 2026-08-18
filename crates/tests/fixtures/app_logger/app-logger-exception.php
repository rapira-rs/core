<?php

// PSR-3 `['exception' => $e]`, chained so the record has to carry more than the outermost frame.
try {
    try {
        throw new \RuntimeException('inner cause', 7);
    } catch (\RuntimeException $prev) {
        throw new \LogicException('outer failure', 42, $prev);
    }
} catch (\LogicException $e) {
    \Rapira\log('boom', \Rapira\LogLevel::Error, ['exception' => $e, 'order' => 'A-1']);
}

echo 'logged';
