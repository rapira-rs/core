# Rapira

[![CI](https://github.com/rapira-rs/rapira/actions/workflows/ci.yml/badge.svg)](https://github.com/rapira-rs/rapira/actions/workflows/ci.yml) [![codecov](https://codecov.io/gh/rapira-rs/rapira/graph/badge.svg)](https://app.codecov.io/gh/rapira-rs/rapira) [![Release](https://img.shields.io/github/v/release/rapira-rs/rapira)](https://github.com/rapira-rs/rapira/releases) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE) [![Docs](https://img.shields.io/badge/docs-rapira.rs-4682b4)](https://rapira.rs)

Rapira is a PHP application server written in Rust. It embeds NTS PHP into the server process through PHP's embed SAPI — the host calls the interpreter directly, with no FastCGI, no sockets, no per-request serialization — and serves HTTP through a bundled Pingora-based front.

Run an existing app unchanged in classic mode (a front controller executed per request, where php-fpm used to sit), or keep it resident in worker mode and pay the bootstrap cost once per worker instead of once per request.

## Documentation

Full documentation lives at **[rapira.rs](https://rapira.rs/docs/)**:

- [Installation](https://rapira.rs/docs/installation)
- [Quickstart](https://rapira.rs/docs/quickstart)
- [Worker mode](https://rapira.rs/docs/worker)
- [Configuration](https://rapira.rs/docs/configuration)
- [Process model](https://rapira.rs/docs/process-model)
- [Framework integration — Symfony, Laravel, Yii3](https://rapira.rs/docs/frameworks/)

## Requirements

- Linux or macOS
- PHP 8.4 or 8.5, NTS — bundled in every release artifact, no separate PHP installation needed

## Install

Grab the package or tarball for your PHP version from [GitHub Releases](https://github.com/rapira-rs/rapira/releases):

```sh
sudo apt install ./rapira-php8.5_<version>_amd64.deb    # Debian/Ubuntu
sudo dnf install ./rapira-php8.5-<version>.x86_64.rpm   # Fedora/RHEL
```

All options, tarballs, checksums and what exactly is bundled: [rapira.rs/docs/installation](https://rapira.rs/docs/installation).

### Docker

`ghcr.io/rapira-rs/rapira` carries the `rapira` binary and the `libphp.so` it was built against, staged for copying into your own image:

```dockerfile
FROM php:8.5-cli-trixie
COPY --from=ghcr.io/rapira-rs/rapira:0.7.0-php8.5 / /
COPY . /app
CMD ["rapira", "serve", "--listen", ":8000", "--mode", "classic", "/app/public/index.php"]
```

The image is `FROM scratch` — it holds `/usr/local/bin/rapira`, `/usr/local/lib/libphp.so`, and opcache (a separate `opcache.so` plus its ini, or linked into libphp, depending on the base). `/usr/local/share/rapira` carries `PHP_VERSION.txt`, the patch level of the bundled libphp, and `debian-packages.txt`, the packages that libphp needs when the base ships no PHP:

```dockerfile
FROM debian:trixie-slim
COPY --from=ghcr.io/rapira-rs/rapira:0.7.0-php8.5 /usr/local/share/rapira/debian-packages.txt /tmp/
RUN apt-get update && xargs -a /tmp/debian-packages.txt apt-get install -y --no-install-recommends
COPY --from=ghcr.io/rapira-rs/rapira:0.7.0-php8.5 / /
```

Extensions are the consumer's to add — adding one rebuilds neither `libphp.so` nor rapira. On a PHP base, `RUN docker-php-ext-install …` is enough. On a base without PHP, build them in a stage on the matching minor and copy them across:

```dockerfile
FROM php:8.5-cli-trixie AS ext
RUN docker-php-ext-install -j"$(nproc)" pdo_mysql

FROM debian:trixie-slim
COPY --from=ghcr.io/rapira-rs/rapira:0.7.0-php8.5 / /
COPY --from=ext /usr/local/lib/php/extensions/ /usr/local/lib/php/extensions/
COPY --from=ext /usr/local/etc/php/conf.d/ /usr/local/etc/php/conf.d/
```

Every tag names its PHP minor — `0.7.0-php8.5`, `0.7-php8.5`, `php8.5`, and the same three for `php8.4` — each covering amd64 and arm64. There is no `latest`: rapira binds Zend structures at compile time, so it checks the linked `libphp.so` at boot and refuses to start against another minor, and a tag that hid which one it carried would only produce containers that fail to boot.

## A taste

```php
<?php
// worker.php — booted once, serving many requests
require __DIR__ . '/vendor/autoload.php';

$app = new App(); // resident: survives across requests

$handler = static function () use ($app): void {
    echo $app->handle($_SERVER['REQUEST_URI']);
};

while (\Rapira\handle_request($handler)) {
}
```

```sh
rapira serve --mode worker worker.php
curl http://127.0.0.1:8000/
```

Front-controller apps run unchanged: `rapira serve --mode classic public/index.php` — see [classic mode](https://rapira.rs/docs/classic).

## Contributing

Build and test instructions are in [CONTRIBUTING.md](https://github.com/rapira-rs/rapira/blob/main/CONTRIBUTING.md). The documentation site is maintained in [rapira-rs/rapira-rs.github.io](https://github.com/rapira-rs/rapira-rs.github.io).

## License

[MIT](LICENSE)
