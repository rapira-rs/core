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
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Install the global subscriber. Call once, after config resolution.
pub fn init(log: &LogSettings) -> anyhow::Result<()> {
    let filter = build_filter(std::env::var("RUST_LOG").ok().as_deref(), log)?;
    let ansi = ansi_enabled(
        io::stderr().is_terminal(),
        std::env::var_os("NO_COLOR").as_deref(),
    );
    tracing_subscriber::registry()
        .with(filter)
        .with(make_layer(log.format, ansi, io::stderr))
        .init();
    Ok(())
}

/// Color only on a tty — a redirected log file must not fill with ANSI
/// sequences — and only with NO_COLOR unset or empty: any non-empty value
/// disables color, an empty one counts as unset. https://no-color.org/
fn ansi_enabled(stderr_is_tty: bool, no_color: Option<&std::ffi::OsStr>) -> bool {
    stderr_is_tty && no_color.is_none_or(|v| v.is_empty())
}

/// A non-blank `RUST_LOG` replaces the config filter wholesale, parsed lossily
/// (a bad directive is dropped, as in env_logger). The config spec parses
/// strictly: a directive the validator accepted must never be silently
/// ignored, and parse_lossy reports drops with a raw eprintln! that the json
/// format never shapes.
fn build_filter(rust_log: Option<&str>, log: &LogSettings) -> anyhow::Result<EnvFilter> {
    match rust_log {
        Some(s) if !s.trim().is_empty() => Ok(EnvFilter::new(s)),
        _ => {
            let mut spec = log.level.as_str().to_owned();
            for (target, level) in &log.targets {
                spec += &format!(",{target}={}", level.as_str());
            }
            EnvFilter::builder()
                .parse(&spec)
                .with_context(|| format!("log filter `{spec}`"))
        }
    }
}

fn make_layer<S, W>(format: LogFormat, ansi: bool, writer: W) -> Box<dyn Layer<S> + Send + Sync>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    match format {
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(false)
            .with_current_span(false)
            .with_span_list(false)
            .with_timer(ChronoUtc::new("%Y-%m-%dT%H:%M:%S%.3fZ".into()))
            .with_file(false)
            .with_line_number(false)
            .with_writer(writer)
            .boxed(),
        LogFormat::Plain => tracing_subscriber::fmt::layer()
            .with_ansi(ansi)
            .with_writer(writer)
            .boxed(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rapira_config::LogLevel;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    /// Captures layer output for assertions; one buffer shared across writers.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Sink {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).expect("utf8 log output")
        }
    }

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Sink {
            self.clone()
        }
    }

    fn settings(level: LogLevel, targets: &[(&str, LogLevel)]) -> LogSettings {
        LogSettings {
            level,
            format: LogFormat::Plain,
            targets: targets
                .iter()
                .map(|(t, l)| (t.to_string(), *l))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    /// Run `emit` under a subscriber built exactly like `init` builds it
    /// (filter + plain uncolored layer) and return what reached the writer.
    fn captured(filter: EnvFilter, emit: impl Fn()) -> String {
        let sink = Sink::default();
        let sub = tracing_subscriber::registry().with(filter).with(make_layer(
            LogFormat::Plain,
            false,
            sink.clone(),
        ));
        tracing::subscriber::with_default(sub, emit);
        sink.text()
    }

    fn emit_probe_events() {
        tracing::error!(target: "rapira", "rapira-error-mark");
        tracing::info!(target: "rapira", "rapira-info-mark");
        tracing::warn!(target: "php", "php-warn-mark");
        tracing::info!(target: "php", "php-info-mark");
        tracing::warn!(target: "php_sys", "php-scoped-warn-mark");
    }

    #[test]
    fn config_spec_filters_by_level_and_target_prefix() {
        let log = settings(LogLevel::Error, &[("php", LogLevel::Warn)]);
        let out = captured(build_filter(None, &log).unwrap(), emit_probe_events);
        assert!(out.contains("rapira-error-mark"));
        assert!(
            out.contains("php-warn-mark"),
            "target override lost:\n{out}"
        );
        // Directives match by raw prefix: `php` also covers `php_sys`, the
        // documented example for [log.targets].
        assert!(
            out.contains("php-scoped-warn-mark"),
            "prefix match lost:\n{out}"
        );
        assert!(!out.contains("rapira-info-mark"));
        assert!(!out.contains("php-info-mark"));
    }

    #[test]
    fn rust_log_replaces_the_config_spec_wholesale() {
        // Under the config spec php-info is hidden (php=warn); RUST_LOG=info
        // must win for the whole filter, not merge with the target override.
        let log = settings(LogLevel::Error, &[("php", LogLevel::Warn)]);
        let out = captured(build_filter(Some("info"), &log).unwrap(), emit_probe_events);
        assert!(
            out.contains("php-info-mark"),
            "config directive survived:\n{out}"
        );
        assert!(out.contains("rapira-info-mark"));
    }

    #[test]
    fn blank_rust_log_falls_back_to_the_config_spec() {
        let log = settings(LogLevel::Error, &[("php", LogLevel::Warn)]);
        let out = captured(build_filter(Some("  "), &log).unwrap(), emit_probe_events);
        assert!(out.contains("php-warn-mark"));
        assert!(!out.contains("php-info-mark"));
    }

    #[test]
    fn invalid_rust_log_drops_the_bad_directive_and_keeps_the_rest() {
        // The survivor must be `warn`, not `error`: a spec whose directives
        // were ALL dropped falls back to EnvFilter::new's implicit error
        // default and would look identical to a kept `error`.
        let log = settings(LogLevel::Trace, &[]);
        let filter = build_filter(Some("!!!,warn"), &log).expect("lossy, never an error");
        let out = captured(filter, emit_probe_events);
        assert!(
            out.contains("php-warn-mark"),
            "surviving directive lost:\n{out}"
        );
        assert!(
            !out.contains("rapira-info-mark"),
            "dropped directive must not widen the filter"
        );
    }

    #[test]
    fn config_target_that_breaks_the_grammar_fails_loudly() {
        // The config validator rejects such keys; if one ever slips past, the
        // strict parse must fail naming the spec instead of silently dropping
        // the directive.
        let log = settings(LogLevel::Error, &[(".php", LogLevel::Warn)]);
        let err = build_filter(None, &log).unwrap_err();
        assert!(err.to_string().contains("error,.php=warn"), "{err:#}");
    }

    #[test]
    fn config_levels_are_valid_filter_directives() {
        for level in [
            LogLevel::Error,
            LogLevel::Warn,
            LogLevel::Info,
            LogLevel::Debug,
            LogLevel::Trace,
        ] {
            let filter = build_filter(None, &settings(level, &[])).unwrap();
            assert_eq!(filter.to_string(), level.as_str());
        }
    }

    #[test]
    fn json_layer_emits_flat_single_line_records() {
        let sink = Sink::default();
        let sub = tracing_subscriber::registry()
            .with(EnvFilter::new("info"))
            .with(make_layer(LogFormat::Json, false, sink.clone()));
        tracing::subscriber::with_default(sub, || {
            tracing::info_span!("req", rid = 1).in_scope(|| {
                tracing::info!(target: "rapira", answer = 42, "boot-mark");
            });
        });
        let out = sink.text();
        // "{"timestamp":"2026-08-01T12:12:54.218Z","level":"INFO","fields":{"message":"boot-mark","answer":42},"target":"rapira"}"
        let line = out.lines().next().expect("one record");
        let v: serde_json::Value = serde_json::from_str(line).expect("json record");
        assert_eq!(v["fields"]["message"], "boot-mark");
        assert_eq!(
            v["fields"]["answer"], 42,
            "event fields must be flattened to the top level"
        );
        assert_eq!(v["target"], "rapira");
        assert_eq!(v["level"], "INFO");
        // ChronoUtc %.3f: RFC 3339 UTC with exactly milliseconds.
        let ts = v["timestamp"].as_str().expect("timestamp");
        assert_eq!(ts.len(), "2026-01-01T00:00:00.000Z".len(), "{ts}");
        assert_eq!(&ts[19..20], ".");
        assert!(ts.ends_with('Z'));
        assert!(v.get("span").is_none() && v.get("spans").is_none());
    }

    #[test]
    fn no_color_counts_as_set_only_when_non_empty() {
        use std::ffi::OsStr;
        assert!(ansi_enabled(true, None));
        assert!(
            ansi_enabled(true, Some(OsStr::new(""))),
            "empty NO_COLOR counts as unset"
        );
        assert!(!ansi_enabled(true, Some(OsStr::new("1"))));
        assert!(!ansi_enabled(false, None), "never color a non-tty");
    }

    #[test]
    fn plain_layer_colors_only_when_asked() {
        // with_ansi overrides the layer default unconditionally, so the ansi
        // argument — not the environment — must decide both ways.
        for (ansi, want_escape) in [(false, false), (true, true)] {
            let sink = Sink::default();
            let sub = tracing_subscriber::registry()
                .with(EnvFilter::new("info"))
                .with(make_layer(LogFormat::Plain, ansi, sink.clone()));
            tracing::subscriber::with_default(sub, || {
                tracing::info!(target: "rapira", "color-mark");
            });
            let out = sink.text();
            assert!(out.contains("color-mark"));
            assert_eq!(out.contains('\u{1b}'), want_escape, "ansi={ansi}:\n{out}");
        }
    }
}
