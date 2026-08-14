<?php
// The bounded-worker pattern: serve one job, end the script with the channel
// still open. The cycle must classify Recycle and re-bootstrap.
\Rapira\handle_request(static function (): void {
    echo 'once';
});
\Rapira\log('one-turn-done');
