//! Config-driven logger: installs the tracing subscriber with the `[log]`
//! filter and format. rapira's crates emit native `tracing` events; upstream
//! pingora still logs through the `log` facade, which `init()` bridges
//! automatically (tracing-subscriber's default `tracing-log` feature installs
//! `LogTracer` and sets the facade's max level from the filter). `RUST_LOG`,
//! when set, replaces the configured filter wholesale — rapira's one
//! env-beats-config knob; it never affects the format.

use rapira_config::{LogFormat, LogSettings};
use std::io::{self, IsTerminal};
use tracing_subscriber::fmt::time::ChronoUtc;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Install the global subscriber. Call once, after config resolution.
pub fn init(log: &LogSettings) {
    // A non-blank RUST_LOG replaces the config filter wholesale.
    let spec = match std::env::var("RUST_LOG") {
        Ok(s) if !s.trim().is_empty() => s,
        _ => {
            let mut s = log.level.as_str().to_owned();
            // parse cases like `php=info,rapira=debug`
            for (target, level) in &log.targets {
                s += &format!(",{target}={}", level.as_str());
            }
            s
        }
    };
    let filter = EnvFilter::new(spec);
    let layer = match log.format {
        // flatten_event emits duplicate JSON keys on field-name collisions:
        // never name a tracing field `timestamp`, `level`, `message` or
        // `target`. Records still arriving over the log bridge (pingora) add
        // `log.*` caller fields; ours are native and stay clean.
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .with_span_list(false)
            .with_timer(ChronoUtc::new("%Y-%m-%dT%H:%M:%S%.3fZ".into()))
            .with_writer(io::stderr)
            .boxed(),
        // tracing honors NO_COLOR but not tty-ness; without the is_terminal
        // gate a redirected log file fills with ANSI sequences.
        LogFormat::Plain => tracing_subscriber::fmt::layer()
            .with_ansi(io::stderr().is_terminal())
            .with_writer(io::stderr)
            .boxed(),
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .init();
}
