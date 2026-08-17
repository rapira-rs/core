# Examples

One file per run mode - two for the dispatcher. Use an installed `rapira`, or build one with `make test_nts` and take `target/nts/debug/rapira`.

Classic - one script execution per request:

```sh
rapira serve --mode classic examples/classic.php
```

Worker - resident script, a handler closure runs per request:

```sh
rapira serve --mode worker examples/worker.php
```

Dispatcher (the default mode) - resident script pulling units from the host, in two flavours. Synchronous, one request at a time on a blocking `receive()`:

```sh
rapira serve examples/dispatcher-sync.php
```

Asynchronous, a fiber per request - `tryReceive()` between resumes while requests are in flight, a blocking `receive()` once none are left:

```sh
rapira serve examples/dispatcher-async.php
```

All of them listen on 127.0.0.1:8000 by default. Classic and worker answer any path; the dispatcher examples route:

```sh
curl http://127.0.0.1:8000/                # hello
curl -d 'ping' http://127.0.0.1:8000/echo  # echoes the request body back
curl http://127.0.0.1:8000/boom            # 500 from a handler failure
curl http://127.0.0.1:8000/nope            # 404 for anything unrouted
curl http://127.0.0.1:8000/stream          # chunked streaming
```
