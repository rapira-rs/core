# core

[![codecov](https://codecov.io/gh/rapira-rs/rapira/graph/badge.svg)](https://app.codecov.io/gh/rapira-rs/rapira)

Rapira - PHP application server. Embeds NTS PHP via the embed SAPI and serves HTTP through the bundled `rapira_pingora` plugin (`crates/plugins/pingora`). This repo contains the SAPI core (`php_sys`), the extension runtime (`rapira_runtime`, `crates/runtime`), and the `rapira` binary. Linux and macOS only.

## Build requirements

- Rust stable (`rust-toolchain.toml` selects the channel)
- C compiler and `pkg-config` (the build compiles `wrapper.c`/`module.c` against the PHP headers)
- libclang for bindgen (`libclang-dev` on Debian/Ubuntu, `clang-devel` on Fedora, `clang` on Arch)
- PHP 8.4 or 8.5, NTS, built with the embed SAPI (`--enable-embed=shared`, provides `libphp.so`; `libphp.dylib` on macOS). ZTS builds are rejected at compile time. `ci/php-configure-flags.txt` has the full configure line used for release builds.
- On 8.4 builds the SAPI registers itself as `fastcgi`, because OPcache on that version starts only for a fixed list of SAPI names; `PHP_SAPI` and `php_sapi_name()` report `rapira` on 8.5+. phpinfo's *Server API* row reads `Rapira` on both.

Install PHP with the embed SAPI from your package manager:

```sh
sudo apt install php8.4-dev libphp8.4-embed   # Debian/Ubuntu (deb.sury.org / ppa:ondrej)
sudo dnf install php-devel php-embedded       # Fedora/RHEL
sudo pacman -S php php-embed                  # Arch
sudo apk add php84-dev php84-embed            # Alpine
```

macOS: Homebrew's `php` has no embed SAPI — build from source with the flags file. Debian/Ubuntu ship only a versioned `libphpX.Y.so`; `make test` and CI symlink it to the plain `libphp.so` the linker wants, a direct `cargo build` needs the same symlink.

PHP is discovered through `php-config`. If it is not the one on `PATH`, point to it explicitly:

```sh
PHP_CONFIG=$HOME/.local/php-nts/bin/php-config cargo build --release
```

At runtime the binary links `libphp.so` (`libphp.dylib` on macOS) dynamically; if it is not in a standard location, point the loader at it:

```sh
LD_LIBRARY_PATH=$HOME/.local/php-nts/lib ./target/release/rapira serve worker.php     # Linux
DYLD_LIBRARY_PATH=$HOME/.local/php-nts/lib ./target/release/rapira serve worker.php   # macOS
```

## Running

```sh
rapira serve [OPTIONS] [SCRIPT]
```

Bare `rapira` prints help. `serve` boots the server from either a `rapira.toml` (`--config`) or turnkey flags. Precedence is **CLI flags > config file > defaults**.

| Option            | Default          | Description                                                                                      |
| ----------------- | ---------------- | ------------------------------------------------------------------------------------------------ |
| `--config <PATH>` | none             | Load settings from a `rapira.toml`.                                                              |
| `--listen <ADDR>` | `127.0.0.1:8000` | Bind address: `host:port`, `:port` (all interfaces), or `unix:<path>`. A bare port is rejected.  |
| `--processes <N>` | CPU count        | Worker processes to fork (static count / max_children for `pool.mode` dynamic & ondemand).       |
| `--classic`       | off              | Re-include the script for every request (front-controller style) instead of keeping it resident. |
| `SCRIPT`          | required¹        | PHP entry script. Overrides `pool.entrypoint`.                                                   |

¹ Required unless the config file sets `pool.entrypoint`.

First `SIGINT`/`SIGTERM` drains in-flight requests and extensions; a second one forces exit.

```sh
rapira serve app/worker.php
curl http://127.0.0.1:8000/
```

### Configuration file

```toml
[http]
listen = "127.0.0.1:8000"
server_name = "localhost"             # optional; SERVER_NAME reported to PHP
server_port = 8000                    # optional; defaults to the listen TCP port (80 for unix:)
max_body_size_mb = 8                  # optional; larger request bodies get a 413
unsafe_field_names = "drop"           # optional; drop (default) | reject

[pool]
entrypoint = "index.php"              # relative → resolved against this file's directory
processes = 4                         # worker processes to fork (max_children for mode = dynamic/ondemand)
classic = false                       # optional; default false
mode = "dynamic"                      # static (default) | dynamic | ondemand
min_spare = 1                         # dynamic only: keep at least this many idle workers
max_spare = 3                         # dynamic only: trim to at most this many idle workers (rejected under other modes)
max_requests = 0                      # recycle a worker after N requests (+jitter); 0 = unlimited
process_idle_timeout_secs = 10        # ondemand: retire an idle worker after this long
request_terminate_timeout_secs = 0    # kill a worker whose single request runs longer (wall clock); 0 = off

[supervisor]                          # optional; master-process policy
pidfile = "/run/rapira.pid"           # optional; relative paths resolve against this file's dir
process_control_timeout_secs = 30     # graceful-stop budget before QUIT → TERM → KILL

[log]                                 # optional; see Logging below
level = "error"                       # error (default) | warn | info | debug | trace
format = "plain"                      # plain (default) | json
```

Unknown keys are rejected. A relative `SCRIPT` on the command line resolves against the current directory; a relative `pool.entrypoint` resolves against the config file's directory.

### Request field names

CGI folds a field name into a `$_SERVER` key by uppercasing it and rewriting `-` to `_`, and PHP rewrites `.` to `_` again when it registers the variable. So `X-Forwarded-For`, `X_Forwarded_For` and `X.Forwarded.For` all reach `$_SERVER['HTTP_X_FORWARDED_FOR']` — which lets a client overwrite a field a trusted proxy in front of rapira set.

`http.unsafe_field_names` decides what happens to a name that is not `[A-Za-z0-9-]`:

- `drop` (default) — the field is removed before PHP sees it, and each removal is logged at `warn`.
- `reject` — the request is answered `400` and nothing is served.

There is no way to turn the screen off. If your clients legitimately send underscore names, rename them to the `-` spelling; a proxy in front of rapira can do the rewrite.

Two other request-field rules follow from the same mapping: a field sent more than once is combined into one value (a comma list, or `"; "` for `Cookie`) before PHP sees it, except for fields whose grammar is a single value — `Authorization`, `Content-Type` and friends keep the first line only. More than one `Host` line is a `400`.

### Process model

rapira runs a **single-threaded master** that binds the listen socket(s), starts PHP once (`MINIT`, so OPcache's SHM is shared with every worker), then forks worker processes — a pre-fork process model. Each worker runs one NTS PHP interpreter behind its own async HTTP runtime and accepts on the inherited socket. The master itself never serves requests; it supervises: it reaps and respawns crashed workers (with backoff), recycles workers after `pool.max_requests`, kills workers whose request exceeds `pool.request_terminate_timeout_secs`, scales the pool for `pool.mode = "dynamic"`/`"ondemand"`, and reloads on `SIGUSR2` by rolling the pool one worker at a time with no dropped connections. `SIGINT`/`SIGTERM` drains gracefully (a second one forces exit). Send signals to the master pid (see `supervisor.pidfile`).

```sh
rapira serve --config /etc/rapira/rapira.toml
```

## Worker script

The resident script asks rapira for a plugin handler, then loops on it. `handleRequest()` blocks until a request arrives, runs the handler with the superglobals (`$_GET`, `$_POST`, `$_SERVER`, `$_COOKIE`, …) populated for that request, and returns `false` when the server is shutting down. State created outside the handler (autoloader, DI container, connections) survives across requests.

```php
<?php
// worker.php
require __DIR__ . '/vendor/autoload.php';

use Rapira\Plugin\Http\HttpHandlerConfig;
use function Rapira\create_plugin_handler;

$http = create_plugin_handler(new HttpHandlerConfig());
$app = new App(); // booted once, reused for every request

$handler = static function () use ($app): void {
    header('Content-Type: text/plain');
    http_response_code(200);
    echo $app->handle($_SERVER['REQUEST_URI']);
};

while ($http->handleRequest($handler)) {
    gc_collect_cycles();
}
```

The config class picks the plugin; `create_plugin_handler()` throws a `Rapira\RapiraException` if no handler matches the config class, or outside worker mode (classic mode has no resident loop). `$http->config->info` describes the plugin the config targets.

`handleRequest()` returns after each request; the loop around it is what runs until shutdown. So one worker script drives one handler — a loop on a second handler is reached only once the first returns `false`.

`$http->getInfo()` returns this worker's live counters — `state`, `pid`, `queued`, `handled`, `errors`, `recycles`, `restarts` — read from its scoreboard slot, except `queued`, which is the current depth of its job intake.

`rapira_finish_request(): bool` flushes the response to the client early; the handler can continue doing work after it (same contract as `fastcgi_finish_request`).

Every class and function rapira exposes is declared in [`crates/php_sys/rapira.stub.php`](crates/php_sys/rapira.stub.php), which doubles as an IDE stub.

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
rapira serve --classic public/index.php
```

## Logging

Logging is configured in the `[log]` section:

```toml
[log]
level = "error"   # error (default) | warn | info | debug | trace
format = "plain"  # plain (default) | json

[log.targets]     # optional; per-target overrides
php = "debug"
pingora_core = "warn"
```

`level` applies to every target; `[log.targets]` raises or lowers individual ones. A key matches by prefix, so `php_sys` covers `php_sys::callbacks` and everything under it.

Log targets:

- `rapira` — server lifecycle: boot, worker lifecycle, shutdown
- `master` — supervision: forks, reaps, respawns, reloads, pool scaling
- `http` — the HTTP front: listeners, request/response field handling, drain
- `ext` — extension task outcomes
- `php` — output and diagnostics coming from PHP itself
- dependencies log under their module paths (`pingora_core`, `tokio`, …)

PHP diagnostics take their level from the error type: fatal errors (`E_ERROR`, `E_PARSE`, `E_CORE_ERROR`, `E_COMPILE_ERROR`, `E_USER_ERROR`, `E_RECOVERABLE_ERROR`) log at `error`, warnings at `warn`, notices at `info`, deprecations at `debug`. A diagnostic excluded by the script's [`error_reporting`](https://www.php.net/manual/en/function.error-reporting.php) mask drops to `trace`, so `error_reporting(E_ALL & ~E_DEPRECATED & ~E_USER_DEPRECATED)` keeps vendor deprecations out of the log. Fatals are never demoted — they explain why a worker recycled — and `E_CORE_ERROR`/`E_CORE_WARNING` are raised before a script can set a mask at all.

Formats, all written to stderr in one write per record, so master and worker output never interleaves mid-record:

- `plain` — `2026-07-30T09:12:34.567890Z ERROR php: …`; colored when stderr is a terminal (`NO_COLOR` turns that off).
- `json` — one object per line: `{"timestamp":…,"level":"ERROR","message":…,"target":…}`. `timestamp` is RFC 3339 UTC with milliseconds, and newlines inside a message are escaped so a record is always one line. Records from the bundled proxy engine add `log.*` caller fields. Never colored.

### `RUST_LOG` (debugging override)

`RUST_LOG`, when set to a non-empty value, replaces `level` and `[log.targets]` entirely — the whole filter, not a merge — so a debugging session needs no config edit. It does not affect `format`.

```sh
RUST_LOG=info rapira serve worker.php            # info+ for everything
RUST_LOG=rapira=debug,php=info rapira serve worker.php
RUST_LOG=warn,rapira=trace rapira serve worker.php  # quiet deps, trace the server
```

## Tests

```sh
make test   # PHP from php-config on PATH; override with PHP_CONFIG=/path/to/php-config
```

The embed library is located under the `php-config` prefix automatically (`lib`, `lib64`, `lib/phpXX`; plain or versioned `libphp*.so`, `libphp.dylib` on macOS).
