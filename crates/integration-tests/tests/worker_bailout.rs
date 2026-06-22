//! One boot per process (PHP embed init is a process-global singleton).
//!
//! NOTE on `exit()`: since PHP 8.4 (`Make exit() unwind properly`), `exit()`/`die()` unwind via
//! `zend_throw_unwind_exit()` — an `EG(exception)` marker — NOT a `zend_bailout` longjmp. So rapira's
//! `zend_try` guard does not "catch" it as a bailout; the request just ends gracefully (default 200).
//! The guarantee that actually matters is that the worker SURVIVES and serves the next request.
use integration_tests::{drain, fixture, req};
use php_sys::{Mode, Rapira};

