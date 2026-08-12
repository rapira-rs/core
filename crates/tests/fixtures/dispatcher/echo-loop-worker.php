<?php

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $req = $ex->getRequest();
        $ex->writeHead(200, [
            'content-type' => ['text/plain'],
            'x-rapira-target' => [$req->target],
        ]);
        $ex->writeBody("method={$req->method} body={$req->body}");
    }
} catch (\Rapira\Exception\ClosedException) {
    \Rapira\log('drained');
}
