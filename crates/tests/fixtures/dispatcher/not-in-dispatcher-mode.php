<?php

try {
    \Rapira\get_dispatcher();
    echo "returned\n";
} catch (\Rapira\Exception\NotInDispatcherModeError $e) {
    echo 'class: ', $e::class, "\n";
    echo 'rapira: ', $e instanceof \Rapira\Exception\RapiraThrowable ? 'yes' : 'no', "\n";
    echo 'message: ', $e->getMessage(), "\n";
}

// The RuntimeException family is userland-constructible and must stay catchable by its stock parent.
try {
    throw new \Rapira\Exception\TimeoutException('elapsed');
} catch (\RuntimeException $e) {
    echo 'timeout-as-runtime: ', $e instanceof \Rapira\Exception\RapiraThrowable ? 'yes' : 'no', "\n";
}
echo 'done';
