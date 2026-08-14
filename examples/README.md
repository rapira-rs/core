# Examples

One file per run mode. Use an installed `rapira`, or build one with `make test_nts` and take `target/nts/debug/rapira`.

Classic — one script execution per request:

```sh
rapira serve --mode classic examples/classic.php
```

Worker — resident script, a handler closure runs per request:

```sh
rapira serve --mode worker examples/worker.php
```

Dispatcher (the default mode) — resident script pulling units from the host, here with ten fibers taking turns on the receive loop:

```sh
rapira serve examples/dispatcher.php
```

With a config file instead of flags — `rapira.toml` here runs the dispatcher example (`pool.entrypoint` resolves against the config's directory; CLI flags override file values):

```sh
rapira serve --config examples/rapira.toml
```

All of the above listen on 127.0.0.1:8000:

```sh
curl http://127.0.0.1:8000/
```
