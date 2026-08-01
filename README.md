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

## A taste

```php
<?php
// worker.php — booted once, serving many requests
require __DIR__ . '/vendor/autoload.php';

use Rapira\Plugin\Http\HttpHandlerConfig;
use function Rapira\create_plugin_handler;

$http = create_plugin_handler(new HttpHandlerConfig());
$app  = new App(); // resident: survives across requests

$handler = static function () use ($app): void {
    echo $app->handle($_SERVER['REQUEST_URI']);
};

while ($http->handleRequest($handler)) {
}
```

```sh
rapira serve worker.php
curl http://127.0.0.1:8000/
```

Front-controller apps run unchanged: `rapira serve --classic public/index.php` — see [classic mode](https://rapira.rs/docs/classic).

## Contributing

Build and test instructions are in [CONTRIBUTING.md](https://github.com/rapira-rs/rapira/blob/main/CONTRIBUTING.md). The documentation site is maintained in [rapira-rs/rapira-rs.github.io](https://github.com/rapira-rs/rapira-rs.github.io).

## License

[MIT](LICENSE)
