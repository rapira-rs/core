//! End-to-end lifecycle suite: boots the real `rapira` binary and exercises the
//! fork-based master over HTTP and Unix signals.

mod harness;
mod lifecycle;
mod reload;
mod scaling;
