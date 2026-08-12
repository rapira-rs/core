<?php

// Echoes every Request field into the response body for the field-mapping test.

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $req = $ex->getRequest();
        $lines = [
            'method=' . $req->method,
            'uri=' . $req->uri,
            'target=' . $req->target,
            'authority=' . var_export($req->authority, true),
            'protocol=' . $req->protocol,
            'x-probe=' . implode(',', $req->headers['x-probe'] ?? []),
            // an all-digit field name must land as an integer key (symtable)
            'h123=' . implode(',', $req->headers[123] ?? []),
            'body=' . $req->body,
            'remote=' . $req->remote::class,
            'remote-ip=' . ($req->remote instanceof \Rapira\InetAddress ? $req->remote->ip : ''),
            'remote-port=' . ($req->remote instanceof \Rapira\InetAddress ? $req->remote->port : ''),
            'tls-null=' . var_export($req->tls === null, true),
            'received-at-positive=' . var_export($req->receivedAt > 0, true),
        ];
        $ex->writeBody(implode("\n", $lines));
    }
} catch (\Rapira\Exception\ClosedException) {
}
