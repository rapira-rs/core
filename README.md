# core

[![codecov](https://codecov.io/gh/rustatian/rapira-rs/graph/badge.svg)](https://app.codecov.io/gh/rustatian/rapira-rs)
![CodeRabbit Pull Request Reviews](https://img.shields.io/coderabbit/prs/github/rustatian/rapira-rs?utm_source=oss&utm_medium=github&utm_campaign=rustatian%2Frapira-rs&labelColor=171717&color=FF570A&link=https%3A%2F%2Fcoderabbit.ai&label=CodeRabbit+Reviews)


Rapira - PHP application server. Embeds PHP (ZTS or NTS) via the embed SAPI, runs requests on a pool of PHP worker threads, and serves HTTP through the bundled `rapira_pingora` plugin (`crates/plugins/pingora`). This repo contains the SAPI core (`php_sys`), the extension host runtime, and the `rapira` binary.

## Build requirements

- Rust 1.97.1+ (stable; `rust-toolchain.toml` pins the channel)
- C compiler and `pkg-config` (the build compiles `wrapper.c`/`module.c` against the PHP headers)
- libclang for bindgen (`libclang-dev` on Debian/Ubuntu, `clang-devel` on Fedora, `clang` on Arch)
- PHP 8.4 or 8.5 built with the embed SAPI (`--enable-embed=shared`, provides `libphp.so`). ZTS (`--enable-zts`) is required for more than one worker thread; on an NTS build rapira runs single-threaded. `ci/php-configure-flags.txt` has the full configure line used for release builds.

PHP is discovered through `php-config`. If it is not the one on `PATH`, point to it explicitly:

```sh
PHP_CONFIG=$HOME/.local/php-zts/bin/php-config cargo build --release
```

On Windows, extract a PHP devel pack and set `PHP_DEVEL_DIR` to its root (plus `LIBCLANG_PATH` to the LLVM `bin` directory).

At runtime the binary links `libphp.so` dynamically; if it is not in a standard location, set `LD_LIBRARY_PATH`:

```sh
LD_LIBRARY_PATH=$HOME/.local/php-zts/lib ./target/release/rapira serve worker.php
```

## Running

```sh
rapira serve [OPTIONS] [SCRIPT]
```

Bare `rapira` prints help. `serve` boots the server from either a `rapira.toml`
(`--config`) or turnkey flags. Precedence is **CLI flags > config file > defaults**.

| Option | Default | Description |
|---|---|---|
| `--config <PATH>` | none | Load settings from a `rapira.toml`. |
| `--listen <ADDR>` | `127.0.0.1:8000` | Bind address: `host:port`, `:port` (all interfaces), or `unix:<path>`. A bare port is rejected. |
| `--threads <N>` | CPU count | PHP worker threads. ZTS only; an NTS build always uses 1. |
| `--classic` | off | Re-include the script for every request (front controller, like PHP-FPM) instead of keeping it resident. |
| `SCRIPT` | required¹ | PHP entry script. Overrides `pool.entrypoint`. |

¹ Required unless the config file sets `pool.entrypoint`.

First `SIGINT`/`SIGTERM` drains in-flight requests and extensions; a second one forces exit.

```sh
rapira serve app/worker.php --threads 8
curl http://127.0.0.1:8000/
```

### Configuration file

```toml
[http]
listen = "127.0.0.1:8000"
server_name = "localhost"    # optional; SERVER_NAME reported to PHP
server_port = 8000           # optional; defaults to the listen TCP port (80 for unix:)
max_body_size_mb = 8         # optional; larger request bodies get a 413

[pool]
threads = 4
entrypoint = "index.php"     # relative → resolved against this file's directory
classic = false              # optional; default false
```

Unknown keys are rejected. A relative `SCRIPT` on the command line resolves against the
current directory; a relative `pool.entrypoint` resolves against the config file's directory.

```sh
rapira serve --config /etc/rapira/rapira.toml
```

## Worker script

The resident script calls `rapira_handle_request(callable): bool` in a loop. Each call blocks until a request arrives, runs the handler with the superglobals (`$_GET`, `$_POST`, `$_SERVER`, `$_COOKIE`, …) populated for that request, and returns `false` when the server is shutting down. State created outside the handler (autoloader, DI container, connections) survives across requests.

```php
<?php
// worker.php
require __DIR__ . '/vendor/autoload.php';

$app = new App(); // booted once, reused for every request

$handler = static function () use ($app): void {
    header('Content-Type: text/plain');
    http_response_code(200);
    echo $app->handle($_SERVER['REQUEST_URI']);
};

while (rapira_handle_request($handler)) {
    gc_collect_cycles();
}
```

`rapira_finish_request(): bool` flushes the response to the client early; the handler can continue doing work after it (same contract as `fastcgi_finish_request`).

## Classic script

In classic mode the script is an ordinary PHP entry point, executed from scratch on every request:

```php
<?php
// index.php
header('Content-Type: text/plain');
echo "Hello, " . ($_GET['name'] ?? 'anonymous') . "!\n";
echo "Method: {$_SERVER['REQUEST_METHOD']}\n";
```

```sh
rapira serve --classic public/index.php --threads 4
```

## Logging

Logging is `env_logger`-based and configured with the `RUST_LOG` environment variable. Without it only errors are printed.

Log targets:

- `rapira` — server lifecycle: boot, worker threads, shutdown
- `ext` — extension task outcomes
- `php` — output and errors coming from PHP itself
- dependencies log under their module paths (`pingora`, `tokio`, …)

Examples:

```sh
RUST_LOG=info rapira serve worker.php            # info+ for everything
RUST_LOG=rapira=debug,php=info rapira serve worker.php
RUST_LOG=warn,rapira=trace rapira serve worker.php  # quiet deps, trace the server
```

Levels: `error`, `warn`, `info`, `debug`, `trace`. `RUST_LOG_STYLE=never` disables colored output.

## Tests

```sh
make test_zts   # against $HOME/.local/php-zts
make test_nts   # against the system NTS PHP (needs php-embedded)
```
