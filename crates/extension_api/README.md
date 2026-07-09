# extension_api — rapira extension SDK

The SDK for writing rapira extensions. An extension is a WebAssembly component that
**drives PHP**: it calls the host's `exec` to run HTTP requests through rapira's PHP
worker pool and gets the response back. Extensions run as sidecar tasks — they don't
serve HTTP themselves.

You write an extension in **its own repository** and depend on this crate over git.
The SDK lives in the rapira core repo alongside the host:

- `crates/extension_api` (+ its `wit/`) — this SDK and the single component interface
  (the `run` export and the `exec` import).
- `crates/extension_cli` (`rapira-extension`) — compiles and packages an extension into `dist/`.
- `examples/hello` — a minimal example extension.

## Write an extension

In your own repo, create a `cdylib` crate that depends on `extension_api` over git:

```toml
# Cargo.toml
[lib]
crate-type = ["cdylib"]

[dependencies]
extension_api = { git = "https://github.com/rapira-rs/core" }
# for join! (running several exec calls concurrently):
futures = { version = "0.3", default-features = false, features = ["async-await"] }
```

Implement `Extension` and register it:

```rust
use extension_api::{Extension, Request, Result, exec, register_extension};

struct Hello;

impl Extension for Hello {
    fn new() -> Self {
        Hello
    }

    async fn run(&mut self) -> Result<()> {
        let res = exec(Request::get("/?from=hello")).await?;
        if res.status != 200 {
            return Err(format!("expected 200, got {}", res.status));
        }
        Ok(())
    }
}

register_extension!(Hello);
```

- `run` is the single entry rapira calls. Return `Err(String)` to report a failure.
- `exec(req).await` runs one request through PHP. It's async — use `futures::join!`
  to run several concurrently (see `examples/hello`).
- `Request { method, uri, headers, body }` / `Response { status, headers, body }`;
  `Request::get(uri)` is a shortcut for a bodyless GET.

Add an `extension.toml` next to `Cargo.toml`:

```toml
id = "hello"          # required; [a-z][a-z0-9_-]{0,63}, and the install dir name
name = "Hello"        # optional human label
```

The host reads only `id` (and `name`); the API version is baked into the `.wasm`
from the crate version (the `rapira:api-version` stamp), so there is no
`api_version`/`version` key to set here — bumping one would have no effect.

## Build

Install the packaging CLI once:

```sh
cargo install --git https://github.com/rapira-rs/core rapira-extension
```

Then, from your extension's directory:

```sh
rapira-extension build .
```

This compiles the crate for `wasm32-wasip2` (adding the target via rustup if needed)
and writes the package to `dist/`:

```
dist/
  extension.wasm
  extension.toml
```

(Bare equivalent, without packaging: `cargo build --target wasm32-wasip2 --release`.)

## Install

rapira loads extensions at boot — there is no live install. Put each packaged
extension in its own subdirectory of rapira's extension directory:

```
<ext-dir>/hello/
  extension.wasm
  extension.toml
```

At boot rapira checks the `rapira:api-version` stamp baked into each `extension.wasm`,
loads the component, and runs its `run`.
