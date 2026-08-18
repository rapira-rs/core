use tests::{app_record, app_records};
use tracing::Level;

/// Cycles are cut at the back-edge and raise no PHP diagnostic.
#[test]
fn cycles_are_broken_without_a_diagnostic() {
    let (level, _, ctx) = app_record("app_logger/limits-cycles.php");

    assert_eq!(level, Level::ERROR);
    assert!(
        ctx.contains(r#""objects":{"bar":{"foo":null}}"#),
        "object cycle must be cut at the back-edge: {ctx:?}"
    );
    assert!(
        ctx.contains(r#""arrays":{"x":{"foo":"bar","y":null}}"#),
        "reference cycle must be cut at the back-edge: {ctx:?}"
    );
    assert!(
        ctx.contains(r#""keep":"visible""#),
        "siblings of a cycle must survive: {ctx:?}"
    );
    let phpdiag: Vec<_> = tests::captured()
        .iter()
        .filter(|c| c.target == "php")
        .map(|c| c.message.clone())
        .collect();
    assert!(phpdiag.is_empty(), "cycles must raise nothing: {phpdiag:?}");
}

/// Guards the off-by-one at the 1000-item boundary: the last item must survive.
#[test]
fn a_thousand_items_are_not_truncated() {
    let records = app_records("app_logger/limits-large-array.php");
    let (_, _, ctx) = records
        .iter()
        .find(|(_, m, _)| m == "exactly-1000")
        .expect("the 1000-item record");

    assert!(
        ctx.contains(",1000]"),
        "the last item must survive: {ctx:?}"
    );
    assert!(
        !ctx.contains("aborting normalization"),
        "exactly 1000 items must not be marked as truncated: {ctx:?}"
    );
}

/// An over-cap array must be truncated and carry a marker naming the real size.
#[test]
#[ignore = "needs the context normalizer (Monolog NormalizerFormatter parity)"]
fn large_arrays_are_capped_and_marked() {
    let records = app_records("app_logger/limits-large-array.php");
    let (_, _, ctx) = records
        .iter()
        .find(|(_, m, _)| m == "over-cap")
        .expect("the 2000-item record");

    assert!(
        ctx.contains("Over 1000 items (2000 total), aborting normalization"),
        "an over-cap array must say what was dropped: {ctx:?}"
    );
    assert!(
        !ctx.contains(",1500,"),
        "items past the cap must not be emitted: {ctx:?}"
    );
    assert!(
        ctx.contains(r#""keep":"visible""#),
        "capping one key must not drop its siblings: {ctx:?}"
    );
}

/// A huge scalar must not become an equally huge log record: no cap, no marker.
#[test]
#[ignore = "needs the context normalizer (Monolog NormalizerFormatter parity)"]
fn huge_strings_are_capped() {
    let (_, _, ctx) = app_record("app_logger/limits-huge-string.php");

    assert!(
        ctx.len() < 128 * 1024,
        "one log call must not emit a multi-megabyte record (got {} bytes)",
        ctx.len()
    );
    assert!(
        ctx.contains(r#""keep":"visible""#),
        "truncating one value must not drop its siblings: {ctx:?}"
    );
}

/// Over-deep branches must be marked, not silently cut: PHP_JSON_PARTIAL_OUTPUT_ON_ERROR disables json's depth ceiling (json_encoder.c:192-197).
#[test]
#[ignore = "needs the context normalizer (Monolog NormalizerFormatter parity)"]
fn deep_nesting_is_marked_not_silently_cut() {
    let (_, _, ctx) = app_record("app_logger/limits-deep.php");

    assert!(
        ctx.contains("levels deep, aborting normalization"),
        "a depth cut must say so rather than emitting a bare null: {ctx:?}"
    );
    assert!(
        ctx.contains(r#""keep":"visible""#),
        "a deep branch must not cost its siblings: {ctx:?}"
    );
}
