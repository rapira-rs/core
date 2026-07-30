//! Config-driven logger: installs the tracing subscriber with the `[log]`
//! filter and format. rapira's crates emit native `tracing` events; upstream
//! pingora still logs through the `log` facade, which `init()` bridges
//! automatically (tracing-subscriber's default `tracing-log` feature installs
//! `LogTracer` and sets the facade's max level from the filter). `RUST_LOG`,
//! when set, replaces the configured filter wholesale — rapira's one
//! env-beats-config knob; it never affects the format.

use anyhow::Context;
use rapira_config::{LogFormat, LogSettings};
use std::io::{self, IsTerminal};
use tracing_subscriber::fmt::time::ChronoUtc;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Install the global subscriber. Call once, after config resolution.
pub fn init(log: &LogSettings) -> anyhow::Result<()> {
    // A non-blank RUST_LOG replaces the config filter wholesale, parsed lossily
    // (a bad directive is dropped, as in env_logger). The config spec parses
    // strictly: a directive the validator accepted must never be silently
    // ignored, and parse_lossy reports drops with a raw eprintln! that the json
    // format never shapes.
    let filter = match std::env::var("RUST_LOG") {
        Ok(s) if !s.trim().is_empty() => EnvFilter::new(s),
        _ => {
            let mut spec = log.level.as_str().to_owned();
            for (target, level) in &log.targets {
                spec += &format!(",{target}={}", level.as_str());
            }
            EnvFilter::builder()
                .parse(&spec)
                .with_context(|| format!("log filter `{spec}`"))?
        }
    };
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
        // with_ansi replaces the layer's default ansi value — the only place
        // tracing-subscriber consults NO_COLOR — so both gates are explicit
        // here: color only on a tty, and only with NO_COLOR unset or empty.
        // Without the tty gate a redirected log file fills with ANSI sequences.
        // https://no-color.org/
        LogFormat::Plain => tracing_subscriber::fmt::layer()
            .with_ansi(
                io::stderr().is_terminal()
                    && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
            )
            .with_writer(io::stderr)
            .boxed(),
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .init();
    Ok(())
}
