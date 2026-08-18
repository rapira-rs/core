<?php

// Monolog testFormat: class name becomes a wrapper key, __toString normalizes to that string, a resource renders as a marker.
class PlainNorm
{
    public $foo = 'fooValue';
}

class StringableNorm
{
    public function __toString(): string
    {
        return 'bar';
    }
}

// Monolog testFormatToStringExceptionHandle: a throwing __toString must not escape the logger.
class ToStringError
{
    public function __toString(): string
    {
        throw new \RuntimeException('Could not convert to string');
    }
}

$fh = fopen('php://memory', 'rb');

\Rapira\log('objects', \Rapira\LogLevel::Error, [
    'plain' => new PlainNorm(),
    'stringable' => new StringableNorm(),
    'boom' => new ToStringError(),
    'res' => $fh,
    'keep' => 'visible',
]);

fclose($fh);
echo 'logged';
