//! End-to-end lifecycle suite: boots the real `rapira` binary and exercises the
//! fork-based master over HTTP and Unix signals. Every test is `#[ignore]`d until
//! the fork/master implementation lands; remove the ignores after wiring.

mod harness;
mod lifecycle;
mod reload;
mod scaling;
