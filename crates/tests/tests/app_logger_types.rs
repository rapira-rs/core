//! How individual context values normalize, adopted from Monolog's
//! `NormalizerFormatterTest::testFormat` and friends.
//!
//! The `#[ignore]`d tests assert Monolog's guarantees, not ours. Each one names a
//! value that today either loses its type or vanishes outright.

use tests::app_record;
use tracing::Level;

/// Monolog testFormat: INF, -INF and NAN normalize to the strings "INF", "-INF"
/// and "NaN". Measured today: all three encode as `0`, which is the worst kind of
/// wrong — a plausible number where a sentinel belongs.
#[test]
#[ignore = "needs the context normalizer (Monolog NormalizerFormatter parity)"]
fn special_floats_keep_their_meaning() {
    let (_, _, ctx) = app_record("app_logger/types-scalars.php");

    for fragment in [r#""inf":"INF""#, r#""ninf":"-INF""#, r#""nan":"NaN""#] {
        assert!(
            ctx.contains(fragment),
            "a non-finite float must not become a number: no {fragment} in {ctx:?}"
        );
    }
}

/// Monolog testIgnoresInvalidEncoding: undecodable bytes are replaced, the value
/// survives. Measured today: the whole value becomes `null`, so raw request bytes
/// disappear entirely rather than degrading.
#[test]
#[ignore = "needs the context normalizer (Monolog NormalizerFormatter parity)"]
fn invalid_utf8_is_substituted_not_dropped() {
    let (_, _, ctx) = app_record("app_logger/types-scalars.php");

    assert!(
        ctx.contains(r#""bad_utf8":"\u{fffd}1""#) || ctx.contains(r#""bad_utf8":"\\ufffd1""#),
        "bad bytes must degrade to U+FFFD, keeping the rest of the string: {ctx:?}"
    );
}

/// Monolog testFormat: an object is wrapped in its class name —
/// `{"Monolog\\Formatter\\TestFooNorm":{"foo":"fooValue"}}`. We emit the bare
/// property bag, so nothing in the record says what type it was.
#[test]
#[ignore = "needs the context normalizer (Monolog NormalizerFormatter parity)"]
fn objects_keep_their_class_name() {
    let (_, _, ctx) = app_record("app_logger/types-objects.php");

    assert!(
        ctx.contains(r#""plain":{"PlainNorm":{"foo":"fooValue"}}"#),
        "an object must carry its class: {ctx:?}"
    );
}

/// Monolog testFormat: `TestBarNorm` has only __toString, and normalizes to
/// `'bar'`. We never call it, so such an object logs as an empty property bag.
#[test]
#[ignore = "needs the context normalizer (Monolog NormalizerFormatter parity)"]
fn stringable_objects_use_their_string_form() {
    let (_, _, ctx) = app_record("app_logger/types-objects.php");

    assert!(
        ctx.contains(r#""stringable":"bar""#),
        "__toString is the only meaningful view of such an object: {ctx:?}"
    );
}

/// Monolog testFormat: a stream renders as `[resource(stream)]`. We emit `null`,
/// which is indistinguishable from a value that really was null.
#[test]
#[ignore = "needs the context normalizer (Monolog NormalizerFormatter parity)"]
fn resources_render_as_a_type_marker() {
    let (_, _, ctx) = app_record("app_logger/types-objects.php");

    assert!(
        ctx.contains(r#""res":"[resource(stream)]""#),
        "a resource must be distinguishable from null: {ctx:?}"
    );
}

/// Monolog testFormatToStringExceptionHandle: an object whose __toString throws
/// degrades to an empty value and the record is still produced. This passes today
/// only because we never call __toString — it must keep passing once we do, which
/// is exactly why it is not ignored.
#[test]
fn a_throwing_tostring_cannot_break_logging() {
    let (level, msg, ctx) = app_record("app_logger/types-objects.php");

    assert_eq!(level, Level::ERROR);
    assert_eq!(msg, "objects");
    assert!(
        ctx.contains(r#""boom""#),
        "the throwing key must still be present: {ctx:?}"
    );
    assert!(
        ctx.contains(r#""keep":"visible""#),
        "a throwing __toString must not cost its siblings: {ctx:?}"
    );
    assert!(
        !ctx.contains("Could not convert to string"),
        "the exception must not escape into the record: {ctx:?}"
    );
}
