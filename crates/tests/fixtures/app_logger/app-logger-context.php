<?php

\Rapira\log('ctx-absent');
\Rapira\log('ctx-empty', \Rapira\LogLevel::Info, []);
\Rapira\log('ctx-full', \Rapira\LogLevel::Info, [
    'route' => '/orders',
    'tries' => 3,
    'ok' => false,
    'nested' => ['id' => 42],
]);

echo 'logged';
