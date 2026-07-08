//! Per-`Store` host state and the sandbox.

use php_sys::RapiraHandle;
use std::sync::Arc;
use wasmtime::{StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

pub struct HostState {
    pub table: ResourceTable,
    pub wasi: WasiCtx,
    pub ext_id: Arc<str>,
    /// The extension drives PHP by submitting requests through this handle.
    pub rapira: RapiraHandle,
    pub limits: StoreLimits,
    stderr: MemoryOutputPipe,
    stderr_logged: usize,
}

impl HostState {
    pub fn new(ext_id: Arc<str>, rapira: RapiraHandle) -> Self {
        // Guest stderr is buffered here and forwarded to the host log after each call,
        // so a guest panic isn't swallowed by an otherwise-empty `WasiCtx`.
        let stderr: MemoryOutputPipe = MemoryOutputPipe::new(256 * 1024);
        // No preopens/env/net; the WASI linker is still required because a
        // wasm32-wasip2 std guest imports wasi:cli/clocks.
        let wasi = WasiCtxBuilder::new().stderr(stderr.clone()).build();

        Self {
            table: ResourceTable::new(),
            wasi,
            ext_id,
            rapira,
            // A guest may not outgrow this.
            limits: StoreLimitsBuilder::new()
                .memory_size(64 * 1024 * 1024)
                .table_elements(65536)
                .build(),
            stderr,
            stderr_logged: 0,
        }
    }

    /// Forward any newly-written guest stderr to the host log.
    pub fn drain_stderr(&mut self) {
        let contents = self.stderr.contents();
        if contents.len() <= self.stderr_logged {
            return;
        }
        let fresh = &contents[self.stderr_logged..];
        self.stderr_logged = contents.len();
        for line in String::from_utf8_lossy(fresh).lines() {
            if !line.trim().is_empty() {
                log::warn!("[ext {}] {line}", self.ext_id);
            }
        }
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}
