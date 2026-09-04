# Rapira

[![CI](https://github.com/rapira-rs/rapira/actions/workflows/ci.yml/badge.svg)](https://github.com/rapira-rs/rapira/actions/workflows/ci.yml) [![codecov](https://codecov.io/gh/rapira-rs/rapira/graph/badge.svg)](https://app.codecov.io/gh/rapira-rs/rapira) [![Release](https://img.shields.io/github/v/release/rapira-rs/rapira)](https://github.com/rapira-rs/rapira/releases) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE) [![Docs](https://img.shields.io/badge/docs-rapira.rs-4682b4)](https://rapira.rs)
![CodeRabbit Pull Request Reviews](https://img.shields.io/coderabbit/prs/github/rapira-rs/rapira?utm_source=oss&utm_medium=github&utm_campaign=rapira-rs%2Frapira&labelColor=171717&color=FF570A&link=https%3A%2F%2Fcoderabbit.ai&label=CodeRabbit+Reviews)

Rapira is a Rust application server for PHP. It embeds non-thread-safe (NTS) PHP in the server process through the PHP embed SAPI. The server calls the PHP interpreter directly. The connection to PHP does not use FastCGI, sockets, or per-request serialization. The built-in HTTP server handles HTTP requests.

Classic mode runs an application's front controller for each request. Worker and dispatcher modes keep the application in memory. Rapira runs the application bootstrap one time for each worker. Worker mode calls an application handler for each request. Dispatcher mode gives each request to the application as an exchange.

## Documentation

For the full documentation, see **[rapira.rs](https://rapira.rs/docs/)**:

- [Installation](https://rapira.rs/docs/intro/installation)
- [Quickstart](https://rapira.rs/docs/intro/quickstart)
- [Worker mode](https://rapira.rs/docs/worker)
- [Configuration](https://rapira.rs/docs/configuration)
- [Process model](https://rapira.rs/docs/process-model)
- [Framework integration: Symfony, Laravel, and Yii3](https://rapira.rs/docs/frameworks/)

## Requirements

- Linux or macOS.
- PHP 8.4 or PHP 8.5, NTS. Each release artifact includes PHP. You do not need a separate PHP installation.

## Install

Download the package or tar archive for your PHP version from [GitHub Releases](https://github.com/rapira-rs/rapira/releases):

```sh
sudo apt install ./rapira-php8.5_<version>_amd64.deb    # Debian/Ubuntu
sudo dnf install ./rapira-php8.5-<version>.x86_64.rpm   # Fedora/RHEL
```

See [the installation documentation](https://rapira.rs/docs/intro/installation) for all options, tar archives, checksums, and included files.

### Docker

The `ghcr.io/rapira-rs/rapira` image contains the `rapira` binary and its required `libphp.so` file. Copy these files into your image:

```dockerfile
FROM php:8.5-cli-trixie
COPY --from=ghcr.io/rapira-rs/rapira:php8.5 / /
COPY . /app
CMD ["rapira", "serve", "--listen", ":8000", "--mode", "classic", "/app/public/index.php"]
```

The image uses `scratch` as its base. It contains `/usr/local/bin/rapira`, `/usr/local/lib/libphp.so`, and OPcache. The PHP base image determines the OPcache configuration. The image contains OPcache in one of two forms. It contains a separate `opcache.so` file with an INI file, or it contains OPcache in `libphp`. The `/usr/local/share/rapira` directory contains `PHP_VERSION.txt` and `debian-packages.txt`. `PHP_VERSION.txt` gives the patch version of the included `libphp`. `debian-packages.txt` lists the packages that `libphp` requires if the base image does not contain PHP:

```dockerfile
FROM debian:trixie-slim
COPY --from=ghcr.io/rapira-rs/rapira:php8.5 /usr/local/share/rapira/debian-packages.txt /tmp/
RUN apt-get update && xargs -a /tmp/debian-packages.txt apt-get install -y --no-install-recommends
COPY --from=ghcr.io/rapira-rs/rapira:php8.5 / /
```

You can add PHP extensions without a rebuild of `libphp.so` or Rapira. For a PHP base image, use `docker-php-ext-install`. For a base image that does not contain PHP, build the extensions in a stage that uses the same PHP minor version. Some extensions also require system libraries. Install the required runtime packages in the final image. Then, copy the extension and INI files:

```dockerfile
FROM php:8.5-cli-trixie AS ext
RUN docker-php-ext-install -j"$(nproc)" pdo_mysql

FROM debian:trixie-slim
COPY --from=ghcr.io/rapira-rs/rapira:php8.5 / /
COPY --from=ext /usr/local/lib/php/extensions/ /usr/local/lib/php/extensions/
COPY --from=ext /usr/local/etc/php/conf.d/ /usr/local/etc/php/conf.d/
```

Each image tag identifies its PHP minor version. PHP 8.5 tag examples include `0.7.0-php8.5`, `0.7-php8.5`, and `php8.5`. Rapira provides the same tag formats for PHP 8.4. Each tag supports `amd64` and `arm64`. Rapira does not provide a `latest` tag. Rapira binds Zend structures at compile time. When Rapira starts, it verifies the PHP minor version of the linked `libphp.so`. If the PHP minor versions are different, Rapira stops with an error.

## Worker mode example

```php
<?php
// Rapira executes this file one time in each worker.
require __DIR__ . '/vendor/autoload.php';

$app = new App(); // This object remains in memory for all requests.

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

## Classic mode example

```php
<?php
// Rapira executes this file for each request.
require __DIR__ . '/../vendor/autoload.php';

$app = new App();

echo $app->handle($_SERVER['REQUEST_URI']);
```

```sh
rapira serve --mode classic public/index.php
curl http://127.0.0.1:8000/
```

For more information, see [classic mode](https://rapira.rs/docs/classic).

## Dispatcher mode example

```php
<?php
use Rapira\Exception\ClosedException;
use Rapira\Exception\WorkDiscardedException;

$dispatcher = \Rapira\get_dispatcher();

try {
    while (true) {
        $exchange = $dispatcher->receive();

        try {
            $request = $exchange->getRequest();
            $exchange->writeHead(200, ['content-type' => ['text/plain']]);
            $exchange->writeBody("Hello, {$request->target}\n");
        } catch (WorkDiscardedException) {
        }
    }
} catch (ClosedException) {
}
```

```sh
rapira serve --mode dispatcher dispatcher.php
curl http://127.0.0.1:8000/
```

See the [synchronous dispatcher example](examples/dispatcher-sync.php) for routing, streaming, and error handling.

## Contributing

See [CONTRIBUTING.md](https://github.com/rapira-rs/rapira/blob/main/CONTRIBUTING.md) for build and test instructions. The source files for the documentation site are in [rapira-rs/rapira-rs.github.io](https://github.com/rapira-rs/rapira-rs.github.io).

## License

[MIT](LICENSE)
