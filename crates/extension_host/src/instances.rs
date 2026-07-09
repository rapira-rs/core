//! The per-extension driver, run as a task on the shared runtime. Each extension
//! gets its own wasmtime `Store`; while its `run` awaits PHP (via `exec`) the task
//! yields, so many extensions multiplex over a few worker threads. No re-entrancy:
//! the extension tasks and the PHP worker threads are distinct.

use crate::state::HostState;
use crate::wit::ExtensionPre;
use php_sys::RapiraHandle;
use std::sync::Arc;
use wasmtime::{Engine, Store};

/// Instantiate and run one extension to completion. The outcome is logged and
/// returned (the returned form is what `Running::join` surfaces).
pub(crate) async fn drive(
    engine: Engine,
    id: Arc<str>,
    pre: ExtensionPre<HostState>,
    rapira: RapiraHandle,
) -> Result<(), String> {
    let mut store = Store::new(&engine, HostState::new(id.clone(), rapira));
    // Bound the guest's own memory; installed before instantiation.
    store.limiter(|s| &mut s.limits);
    // Preempt a non-yielding guest: hand the ext executor back on each epoch tick
    // (bumped by the ticker in `run`) rather than pin the worker thread.
    store.set_epoch_deadline(1);
    store.epoch_deadline_async_yield_and_update(1);

    // `run` and `exec` are async component funcs, so the guest is driven on the
    // component's concurrent runtime: `run_concurrent` hands the export call an
    // `Accessor`, and the guest's own `join!`ed `exec`s run as concurrent subtasks.
    let outcome: wasmtime::Result<Result<(), String>> =
        match pre.instantiate_async(&mut store).await {
            Ok(bindings) => store
                .run_concurrent(async |accessor| bindings.call_run(accessor).await)
                .await
                .and_then(|inner| inner),
            Err(e) => Err(e),
        };
    store.data_mut().drain_stderr();

    match outcome {
        Ok(Ok(())) => {
            log::info!("[ext {id}] finished");
            Ok(())
        }
        Ok(Err(msg)) => {
            log::error!("[ext {id}] run failed: {msg}");
            Err(msg)
        }
        Err(trap) => {
            let m = format!("trapped: {trap:?}");
            log::error!("[ext {id}] {m}");
            Err(m)
        }
    }
}
