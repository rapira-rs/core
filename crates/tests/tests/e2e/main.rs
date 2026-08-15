//! End-to-end lifecycle suite: boots the real `rapira` binary and exercises the
//! fork-based master over HTTP and Unix signals.

mod concurrency;
mod harness;
mod ini;
mod lifecycle;
mod logging;
mod reload;
mod scaling;
mod streaming;
