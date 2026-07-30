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
    // A blank RUST_LOG counts as unset, so the precedence rule stays
    // "if RUST_LOG has a value it replaces the config filter".
    let spec = match std::env::var("RUST_LOG") {
        Ok(s) if !s.trim().is_empty() => s,
        _ => log.filter_directives(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the boundary between `filter_directives()` output and
    /// EnvFilter's directive grammar.
    #[test]
    fn envfilter_accepts_every_config_filter() {
        use rapira_config::LogLevel;
        use std::collections::BTreeMap;

        let cases = [
            LogSettings {
                level: LogLevel::Error,
                format: LogFormat::Plain,
                targets: BTreeMap::new(),
            },
            LogSettings {
                level: LogLevel::Trace,
                format: LogFormat::Json,
                targets: BTreeMap::from([
                    ("php".to_owned(), LogLevel::Error),
                    ("tokio::net".to_owned(), LogLevel::Debug),
                    ("a".to_owned(), LogLevel::Trace),
                ]),
            },
        ];
        for settings in cases {
            let spec = settings.filter_directives();
            EnvFilter::try_new(&spec).unwrap_or_else(|e| panic!("`{spec}`: {e}"));
        }
    }
}
