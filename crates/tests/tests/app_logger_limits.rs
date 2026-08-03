//! Bounds on what one `\Rapira\log()` call can put on the wire, adopted from
//! Monolog's `NormalizerFormatterTest`.
//!
//! Monolog normalizes before encoding, which is where its caps live; we hand the
//! array straight to `php_json_encode`. The `#[ignore]`d tests below are the
//! executable spec for that missing pass — they assert the behaviour we want, not
//! the behaviour we have.

use tests::app_records;
use tracing::Level;

/// Monolog: testIgnoresRecursiveObjectReferences, testCanNormalizeReferences.
/// Both of its tests install an error handler that fails on any diagnostic, so
/// "no warning raised" is part of the contract, not just "no crash".
#[test]
fn cycles_are_broken_without_a_diagnostic() {
    let records = app_records("app_logger/limits-cycles.php");
    let (level, _, ctx) = records.first().expect("one record");

    assert_eq!(*level, Level::ERROR);
    // The back-edge becomes null; everything up to it survives.
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
    // A cycle must not surface to the app as a PHP diagnostic.
    let phpdiag: Vec<_> = tests::captured()
        .iter()
        .filter(|c| c.target == "php")
        .map(|c| c.message.clone())
        .collect();
    assert!(phpdiag.is_empty(), "cycles must raise nothing: {phpdiag:?}");
}

/// Monolog: testNormalizeHandleLargeArraysWithExactly1000Items. The boundary is
/// the point — this must keep passing once a cap exists, so it guards the
/// off-by-one rather than the cap itself.
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

/// Monolog: testNormalizeHandleLargeArrays — over the cap, the array is truncated
/// and carries a marker naming the real size. Measured today: 2000 items encode
/// in full, and 500_000 produce a 3.39 MB record.
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

/// Beyond Monolog, which caps items and depth but never string length. Measured:
/// a single 5 MiB scalar becomes a 5.24 MB log record with no cap and no marker.
#[test]
#[ignore = "needs the context normalizer (Monolog NormalizerFormatter parity)"]
fn huge_strings_are_capped() {
    let records = app_records("app_logger/limits-huge-string.php");
    let (_, _, ctx) = records.first().expect("one record");

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

/// Monolog: testMaxNormalizeDepth — over-deep branches abort with a marker naming
/// the limit. Measured today: PHP_JSON_PARTIAL_OUTPUT_ON_ERROR disables json's
/// depth ceiling entirely (json_encoder.c:192-197), so encoding runs until Zend's
/// stack guard trips at roughly 5500 levels and substitutes a bare null. Bounded,
/// but silent — and the cut-off moves with how deep the PHP call stack already was.
#[test]
#[ignore = "needs the context normalizer (Monolog NormalizerFormatter parity)"]
fn deep_nesting_is_marked_not_silently_cut() {
    let records = app_records("app_logger/limits-deep.php");
    let (_, _, ctx) = records.first().expect("one record");

    assert!(
        ctx.contains("levels deep, aborting normalization"),
        "a depth cut must say so rather than emitting a bare null: {ctx:?}"
    );
    assert!(
        ctx.contains(r#""keep":"visible""#),
        "a deep branch must not cost its siblings: {ctx:?}"
    );
}
