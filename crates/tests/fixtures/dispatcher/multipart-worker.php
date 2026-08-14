<?php

// Echoes the parsed Multipart shape; the spool-file lifetime is asserted by
// the test (gone once the response frame arrives — seal unlinks first).

$d = \Rapira\get_dispatcher();
try {
    while (true) {
        $ex = $d->receive();
        $req = $ex->getRequest();
        if (!$req->body instanceof \Rapira\Http\Multipart) {
            $ex->writeBody('not-multipart: ' . get_debug_type($req->body));
            continue;
        }
        $b = $req->body;
        $lines = [
            'class=' . $b::class,
            'counts=' . count($b->fields) . '/' . count($b->files),
        ];
        // per-index: a swapped or misaligned part must show up in the output
        foreach ($b->fields as $i => $f) {
            $lines[] = "field$i=" . $f->name . '=' . $f->value;
            $lines[] = "field$i-cd=" . var_export(isset($f->headers['content-disposition']), true);
        }
        foreach ($b->files as $i => $u) {
            $lines[] = "file$i=" . $u->name . ':' . $u->clientFilename . ':' . $u->size
                . ':' . file_get_contents($u->tmpPath);
            $lines[] = "file$i-type=" . var_export($u->clientMediaType, true);
        }
        $ex->writeBody(implode("\n", $lines));
    }
} catch (\Rapira\Exception\ClosedException) {
}
