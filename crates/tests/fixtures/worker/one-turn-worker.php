<?php
// bounded worker: the script ends with the channel still open, so the cycle must classify Recycle and re-bootstrap.
\Rapira\handle_request(static function (): void {
    echo 'once';
});
\Rapira\log('one-turn-done');
