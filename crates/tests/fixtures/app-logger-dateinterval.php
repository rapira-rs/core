<?php

\Rapira\log('date-interval', \Rapira\LogLevel::Error, [
    'fromSpec' => new \DateInterval('P1M2D'),
    'fromDateString' => \DateInterval::createFromDateString('1 month 2 days'),
]);

echo 'logged';
