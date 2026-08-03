//! `\Rapira\log()`: the userland path onto the host's `app` tracing target.

use php_sys::{Mode, Rapira};
use tests::{Captured, captured, drain, init_log_capture, php_lock, req};
use tracing::Level;

/// The `app`-target records left by one run of `script`, in emission order.
fn app_records(script: &str) -> Vec<(Level, String, String)> {
    let _guard = php_lock();
    init_log_capture();
    captured().clear(); // drop anything captured by earlier tests

    let r = Rapira::start(Mode::Classic).expect("classic boot");
    let h = r.handle().expect("handle");
    let (status, body) = drain(h.handle_blocking(req("/", script)).expect("dispatch"));
    drop(h);
    r.shutdown();

    assert_eq!(status, 200, "{script} must run clean (body: {body:?})");
    assert!(body.contains("logged"), "{script} ran to the end: {body:?}");

    captured()
        .iter()
        .filter(|c: &&Captured| c.target == "app")
        .map(|c| (c.level, c.message.clone(), c.context.clone()))
        .collect()
}

/// Every LogLevel case must reach the host at the matching tracing level, and an
/// omitted argument must land on Info.
#[test]
fn log_levels_map_onto_tracing_levels() {
    let records = app_records("app-logger-levels.php");

    let got: Vec<(Level, &str)> = records
        .iter()
        .map(|(lvl, msg, _)| (*lvl, msg.as_str()))
        .collect();

    assert_eq!(
        got,
        vec![
            (Level::ERROR, "lvl-error"),
            (Level::WARN, "lvl-warning"),
            (Level::INFO, "lvl-info"),
            (Level::DEBUG, "lvl-debug"),
            (Level::TRACE, "lvl-trace"),
            // No level argument: the stub default has to be applied in C.
            (Level::INFO, "lvl-omitted"),
        ],
        "each case must map to its own level, in order"
    );
}

/// The context array is JSON-encoded host-side; absent and empty contexts must
/// carry no field at all rather than an empty object.
#[test]
fn log_context_is_json_encoded() {
    let records = app_records("app-logger-context.php");
    let find = |needle: &str| {
        records
            .iter()
            .find(|(_, msg, _)| msg == needle)
            .unwrap_or_else(|| panic!("no {needle:?} record in {records:?}"))
    };

    assert_eq!(find("ctx-absent").2, "", "an omitted context adds no field");
    assert_eq!(find("ctx-empty").2, "", "an empty array adds no field");

    // Exercises the ext/json linkage and that the array survived the C round trip
    // with its scalar types intact.
    let ctx = &find("ctx-full").2;
    for fragment in [
        r#""route":"\/orders""#,
        r#""tries":3"#,
        r#""ok":false"#,
        r#""nested":{"id":42}"#,
    ] {
        assert!(ctx.contains(fragment), "missing {fragment} in {ctx:?}");
    }
}

/// The single context value most likely to appear in real code: PSR-3's
/// `['exception' => $e]`. A Throwable's state lives in private properties of
/// Exception/Error, so nothing that only walks public properties can see it.
#[test]
fn log_context_carries_a_throwable() {
    let records = app_records("app-logger-exception.php");
    let (level, _, ctx) = records.first().expect("one record");
    assert_eq!(*level, Level::ERROR);

    for fragment in [
        "LogicException",           // which class failed
        "outer failure",            // its message
        "42",                       // its code
        "app-logger-exception.php", // where
    ] {
        assert!(
            ctx.contains(fragment),
            "a logged exception must be diagnosable: no {fragment:?} in {ctx:?}"
        );
    }
    // The chained cause is why it failed; the outermost frame alone rarely says.
    assert!(
        ctx.contains("inner cause"),
        "the previous exception must survive: {ctx:?}"
    );
    // Flattening the Throwable must not disturb its siblings.
    assert!(
        ctx.contains(r#""order":"A-1""#),
        "sibling key lost: {ctx:?}"
    );
}

/// log() is called from catch blocks, so a context json_encode cannot represent
/// must not throw, must not drop the record, and must not take its neighbours
/// down with it. The substitute value is php-src's business; presence is ours.
#[test]
fn log_context_tolerates_unencodable_values() {
    let records = app_records("app-logger-unencodable.php");
    let (level, msg, ctx) = records.first().expect("the record must survive");

    assert_eq!(*level, Level::ERROR);
    assert_eq!(msg, "hostile");
    assert!(
        ctx.contains(r#""keep":"visible""#),
        "encodable neighbours must be intact: {ctx:?}"
    );
    for key in ["closure", "resource", "nan", "inf", "bytes", "pure_enum"] {
        assert!(
            ctx.contains(&format!("\"{key}\"")),
            "{key} must still appear rather than being dropped: {ctx:?}"
        );
    }
}

/// The ordinary things that end up in a context: entities, JsonSerializable value
/// objects, backed enums, nulls and nested arrays.
#[test]
fn log_context_encodes_common_php_values() {
    let records = app_records("app-logger-values.php");
    let (_, _, ctx) = records.first().expect("one record");

    for fragment in [
        // Only public state crosses the boundary.
        r#""obj":{"id":"acc_1","note":null}"#,
        // jsonSerialize() wins over the object's own properties.
        r#""money":{"cents":1250}"#,
        // A backed enum encodes as its backing value.
        r#""suit":"H""#,
        r#""nothing":null"#,
        r#""list":[1,2,3]"#,
        r#""deep":{"a":{"b":{"c":"bottom"}}}"#,
        r#""zero":0"#,
    ] {
        assert!(ctx.contains(fragment), "missing {fragment} in {ctx:?}");
    }
    assert!(
        !ctx.contains("private") && !ctx.contains("protected"),
        "non-public properties must not leak: {ctx:?}"
    );
}
