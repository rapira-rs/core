//! Generated bindings for the `rapira:extension@0.1.0` world.
//!
//! `run` (export) and `exec` (import) are WIT `async func`s, so bindgen generates
//! them on the component-model async path (async host fn + concurrent export call).

wasmtime::component::bindgen!({
    world: "extension",
    path: "../extension_api/wit",
});
