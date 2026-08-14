<?php
// Asynchronous dispatcher: a fiber per request. While requests are in flight
// the loop polls tryReceive() between resumes, so it never blocks them; once
// none are left it parks in a blocking receive() — no sleep, no idle spin.

use Rapira\Exception\ClosedException;
use Rapira\Exception\RapiraThrowable;
use Rapira\Http\Exchange;
use Rapira\Http\Request;

final class PageNotFound extends \RuntimeException {}

/** Routes a request to a response body */
function handle(Request $req): Generator|string
{
    return match ($req->target) {
        '/' => "hello from the async dispatcher\n",
        '/echo' => \is_string($req->body) ? $req->body : '',
        '/stream' => generateStream(),
        '/boom' => throw new \RuntimeException('the handler blew up'),
        default => throw new PageNotFound("no route for {$req->target}"),
    };
}

function generateStream(): Generator
{
    $j = mt_rand(5, 20);
    for ($i = 0; $i < $j; $i++) {
        yield "stream chunk {$i}\n";
    }

    return "stream done\n";
}

$serve = static function (Exchange $ex): void {
    try {
        $body = handle($ex->getRequest());
        $ex->writeHead(200, ['content-type' => ['text/plain']]);
        if ($body instanceof Generator) {
            foreach ($body as $chunk) {
                $ex->writeBody($chunk, eos: false);
                Fiber::suspend();
            }

            $ex->writeBody($body->getReturn());
        } else {
            $ex->writeBody($body);
        }
    } catch (RapiraThrowable) {
        // The host closed the exchange first — nothing to answer.
    } catch (PageNotFound $e) {
        try {
            $ex->writeHead(404, ['content-type' => ['text/plain']]);
            $ex->writeBody("not found: {$e->getMessage()}\n");
        } catch (\Throwable) {
        }
    } catch (\Throwable $e) {
        try {
            $ex->writeHead(500, ['content-type' => ['text/plain']]);
            $ex->writeBody("internal error: {$e->getMessage()}\n");
        } catch (\Throwable) {
        }
    }
};

$d = \Rapira\get_dispatcher();

/** @var array<int, Fiber> $fibers */
$fibers = [];
$max = 100;

try {
    while (true) {
        $ex = match (count($fibers)) {
            $max => null,
            0 => $d->receive(),
            default => $d->tryReceive(),
        };

        if ($ex !== null) {
            $fiber = new Fiber($serve);
            $fiber->start($ex);
            $fiber->isTerminated() or $fibers[] = $fiber;
        }

        foreach ($fibers as $i => $fiber) {
            $fiber->resume();
            if ($fiber->isTerminated()) {
                unset($fibers[$i]);
            }
        }
    }
} catch (ClosedException) {
    do {
        foreach ($fibers as $i => $fiber) {
            $fiber->resume();
            if ($fiber->isTerminated()) {
                unset($fibers[$i]);
            }
        }
    } while (count($fibers) > 0);
}
