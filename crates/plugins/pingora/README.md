# rapira_pingora

Rapira HTTP front - a [Pingora](https://github.com/cloudflare/pingora) server that
terminates HTTP and answers every request from PHP through the `extension_api` `Php`
bridge. It never proxies upstream.

## How it works

- Runs as a Rapira extension: `HttpServer` implements `extension_api::Extension`.
- The extension runtime has IO disabled, so Pingora runs on its own IO-enabled
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

`Config` is injected at construction via `Extension::init(config)`: `rapira serve`
resolves CLI flags, an optional `rapira.toml`, and defaults into one validated
settings struct, builds this crate's `Config` from its `[http]` section, and
registers the extension with it (`host.register::<HttpServer>(config)`).

| Field | Default | Purpose |
| --- | --- | --- |
| `listen` | `127.0.0.1:8000` | Bind address: TCP (`host:port`) or unix domain socket (`unix:<path>`) |
| `server_name` | `localhost` | `SERVER_NAME` reported to PHP |
| `server_port` | the listen port | `SERVER_PORT` reported to PHP |
| `max_body_size` | 8 MiB | Max request body; larger is rejected with `413` |

## Build

```sh
cargo build -p rapira_pingora
cargo clippy -p rapira_pingora --all-targets
```

## License

MIT - see the repository root [LICENSE](../../../LICENSE).
