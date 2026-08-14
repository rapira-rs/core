<?php
// Ten fibers take turns on the receive loop: each turn serves one unit, then
// yields. The dispatcher hands out one unit at a time today; this layout is
// what concurrent dispatch uses once receive() suspends the fiber.

use Rapira\Exception\ClosedException;

$d = \Rapira\get_dispatcher();

$fibers = [];
for ($i = 0; $i < 10; $i++) {
    $fibers[$i] = new Fiber(static function () use ($d, $i): void {
        while (true) {
            $ex = $d->receive();
            $req = $ex->getRequest();
            $ex->writeHead(200, ['content-type' => ['text/plain']]);
            $ex->writeBody("fiber $i handled {$req->target}\n");
            Fiber::suspend();
        }
    });
}

try {
    for ($i = 0; true; $i = ($i + 1) % 10) {
        $fibers[$i]->isStarted() ? $fibers[$i]->resume() : $fibers[$i]->start();
    }
} catch (ClosedException) {
}
