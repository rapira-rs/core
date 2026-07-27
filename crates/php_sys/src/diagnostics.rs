//! Severity for the two paths PHP diagnostics reach the log by: the SAPI log callback and the
//! last-error slot drained at worker teardown. The tables live together because they have to
//! agree on deprecations.

use crate::{
    E_COMPILE_WARNING, E_CORE, E_CORE_WARNING, E_DEPRECATED, E_FATAL_ERRORS, E_NOTICE,
    E_USER_DEPRECATED, E_USER_NOTICE, E_USER_WARNING, E_WARNING,
};
use std::os::raw::c_int;

// php-src groups the fatal and core types itself; these three have no upstream equivalent.
const WARNINGS: u32 = E_WARNING | E_CORE_WARNING | E_COMPILE_WARNING | E_USER_WARNING;
const NOTICES: u32 = E_NOTICE | E_USER_NOTICE;
const DEPRECATIONS: u32 = E_DEPRECATED | E_USER_DEPRECATED;

/// Level and label for a `PG(last_error_type)` under the `EG(error_reporting)` mask.
///
/// A masked diagnostic drops to `Trace` instead of disappearing. Fatals are exempt: they are
/// the only account of why a worker recycled.
///
/// The mask is sampled at teardown, while `@` restores it at the end of the silenced statement
/// (Zend/zend_vm_def.h, ZEND_END_SILENCE), so a silenced diagnostic still reports. Worker mode
/// also never runs `zend_ini_deactivate` per job, so a runtime `error_reporting()` persists to
/// later jobs of the cycle. Both only ever over-report a non-fatal.
/// https://www.php.net/manual/en/function.error-reporting.php
pub(crate) fn error_type_to_level(err_type: c_int, mask: c_int) -> (log::Level, &'static str) {
    // php-src stores `orig_type & E_ALL`, so both are non-negative; the bindgen-derived E_*
    // are u32
    let (err_type, mask) = (err_type as u32, mask as u32);
    let (level, label) = match err_type {
        // fatal bits win when more than one is set
        t if t & E_FATAL_ERRORS != 0 => (log::Level::Error, "Fatal error"),
        t if t & WARNINGS != 0 => (log::Level::Warn, "Warning"),
        t if t & NOTICES != 0 => (log::Level::Info, "Notice"),
        t if t & DEPRECATIONS != 0 => (log::Level::Debug, "Deprecated"),
        // a bit added to E_ALL after this table, or 0 for a type outside it. Unrecognized is
        // not known to be benign: keep it visible.
        _ => (log::Level::Warn, "Unknown error"),
    };
    // `err_type != 0` guards the 0 case, which would otherwise always test as masked
    if err_type != 0 && err_type & E_FATAL_ERRORS == 0 && err_type & (mask | E_CORE) == 0 {
        return (log::Level::Trace, label);
    }
    (level, label)
}

pub(crate) fn syslog_to_level(syslog_lev: c_int) -> log::Level {
    match syslog_lev {
        0 => log::Level::Error, // LOG_EMERG
        1 => log::Level::Error, // LOG_ALERT
        2 => log::Level::Error, // LOG_CRIT
        3 => log::Level::Error, // LOG_ERR
        4 => log::Level::Warn,  // LOG_WARNING
        5 => log::Level::Info,  // LOG_NOTICE
        // php-src's priority for E_DEPRECATED/E_USER_DEPRECATED, and nothing else in core
        // reaches this callback with it (main/main.c:1443-1446)
        6 => log::Level::Debug, // LOG_INFO
        7 => log::Level::Debug, // LOG_DEBUG
        _ => log::Level::Info,
    }
}
