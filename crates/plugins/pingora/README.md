# rapira_pingora

[![CodSpeed](https://img.shields.io/endpoint?url=https://codspeed.io/badge.json)](https://app.codspeed.io/rustatian/http?utm_source=badge)

Rapira HTTP front — a [Pingora](https://github.com/cloudflare/pingora) server that
terminates HTTP and answers every request from PHP through the `extension_api` `Php`
bridge. It never proxies upstream.

## How it works

- Runs as a Rapira extension: `HttpServer` implements `extension_api::Extension`.
- The extension host runtime has IO disabled, so Pingora runs on its own IO-enabled
  Tokio runtime on a dedicated thread.
- Each request is mapped to an `extension_api::Request`, executed via `Php::exec`, and
  the response is written back to the client.
- The full PHP response is buffered so a real `Content-Length` can be sent; this keeps
  HTTP/1.1 connections alive instead of close-delimiting every response.
- `shutdown` stops the accept loop and drains in-flight requests (up to 25s) before the
  runtime is dropped.

## Request handling

- Bodies larger than `max_body_size` are rejected with `413`, checked against
  `Content-Length` and again while reading chunked bodies.
- `Expect: 100-continue` is honored for HTTP/1.1 requests.
- Hop-by-hop response headers are stripped, including any field named by a PHP
  `Connection` header ([RFC 9110 §7.6.1](https://www.rfc-editor.org/rfc/rfc9110#section-7.6.1)).
  Framing (`Content-Length`, `Transfer-Encoding`) is owned by the server, not PHP.

## Configuration

Config is currently hardcoded to `Config::default()` — `Extension::init()` takes no
arguments, so there is no channel to inject config yet (planned).

| Field | Default | Purpose |
| --- | --- | --- |
| `listen` | `0.0.0.0:8080` | TCP bind address |
| `server_name` | `localhost` | `SERVER_NAME` reported to PHP |
| `server_port` | `8080` | `SERVER_PORT` reported to PHP |
| `max_body_size` | 8 MiB | Max request body; larger is rejected with `413` |

## Build

```sh
cargo build --release
cargo clippy --all-targets
```

## License

MIT — see [LICENSE](LICENSE).
