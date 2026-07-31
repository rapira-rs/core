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

/// Emit to the `php` target at a runtime level. `tracing::event!` needs a const
/// level (each callsite is a static), so the runtime value fans out over the
/// five arms; the target must be const too, which pins this macro to `php`.
macro_rules! php_log {
    ($lvl:expr, $($arg:tt)+) => {
        match $lvl {
            tracing::Level::ERROR => tracing::event!(target: "php", tracing::Level::ERROR, $($arg)+),
            tracing::Level::WARN => tracing::event!(target: "php", tracing::Level::WARN, $($arg)+),
            tracing::Level::INFO => tracing::event!(target: "php", tracing::Level::INFO, $($arg)+),
            tracing::Level::DEBUG => tracing::event!(target: "php", tracing::Level::DEBUG, $($arg)+),
            tracing::Level::TRACE => tracing::event!(target: "php", tracing::Level::TRACE, $($arg)+),
        }
    };
}
pub(crate) use php_log;

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
pub(crate) fn error_type_to_level(err_type: c_int, mask: c_int) -> (tracing::Level, &'static str) {
    // php-src stores `orig_type & E_ALL`, so both are non-negative; the bindgen-derived E_*
    // are u32
    let (err_type, mask) = (err_type as u32, mask as u32);
    let (level, label) = match err_type {
        // fatal bits win when more than one is set
        t if t & E_FATAL_ERRORS != 0 => (tracing::Level::ERROR, "Fatal error"),
        t if t & WARNINGS != 0 => (tracing::Level::WARN, "Warning"),
        t if t & NOTICES != 0 => (tracing::Level::INFO, "Notice"),
        t if t & DEPRECATIONS != 0 => (tracing::Level::DEBUG, "Deprecated"),
        // a bit added to E_ALL after this table, or 0 for a type outside it. Unrecognized is
        // not known to be benign: keep it visible.
        _ => (tracing::Level::WARN, "Unknown error"),
    };
    // `err_type != 0` guards the 0 case, which would otherwise always test as masked
    if err_type != 0 && err_type & E_FATAL_ERRORS == 0 && err_type & (mask | E_CORE) == 0 {
        return (tracing::Level::TRACE, label);
    }
    (level, label)
}

pub(crate) fn syslog_to_level(syslog_lev: c_int) -> tracing::Level {
    match syslog_lev {
        0 => tracing::Level::ERROR, // LOG_EMERG
        1 => tracing::Level::ERROR, // LOG_ALERT
        2 => tracing::Level::ERROR, // LOG_CRIT
        3 => tracing::Level::ERROR, // LOG_ERR
        4 => tracing::Level::WARN,  // LOG_WARNING
        5 => tracing::Level::INFO,  // LOG_NOTICE
        // php-src's priority for E_DEPRECATED/E_USER_DEPRECATED, and nothing else in core
        // reaches this callback with it (main/main.c:1443-1446)
        6 => tracing::Level::DEBUG, // LOG_INFO
        7 => tracing::Level::DEBUG, // LOG_DEBUG
        _ => tracing::Level::INFO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;

    // E_ERROR is outside the bindgen allowlist; it is the lowest bit of php-src's fatal group.
    // Taking that bit rather than the whole group keeps E_CORE_ERROR out of the mask cases.
    const E_ERROR: u32 = 1 << E_FATAL_ERRORS.trailing_zeros();

    fn level_of(err_type: u32, mask: u32) -> (Level, &'static str) {
        error_type_to_level(err_type as c_int, mask as c_int)
    }

    /// The fatal arm is matched before the warning arm, and the mask clause skips fatals: the
    /// reason a worker recycled has to survive even an `error_reporting(0)`.
    #[test]
    fn fatals_outrank_a_warning_bit_and_ignore_the_mask() {
        assert_eq!(
            level_of(E_ERROR | E_WARNING, E_WARNING),
            (Level::ERROR, "Fatal error")
        );
        assert_eq!(level_of(E_ERROR, 0), (Level::ERROR, "Fatal error"));
    }

    /// A masked non-fatal drops to `Trace` with its label intact, so raising the level shows
    /// what was silenced instead of an unlabelled line.
    #[test]
    fn a_masked_warning_drops_to_trace_with_its_label() {
        assert_eq!(level_of(E_USER_WARNING, 0), (Level::TRACE, "Warning"));
        assert_eq!(
            level_of(E_USER_WARNING, E_USER_WARNING),
            (Level::WARN, "Warning")
        );
    }

    /// `mask | E_CORE` exempts core diagnostics, which the sampled `EG(error_reporting)` does
    /// not describe, so an empty mask must not demote them.
    #[test]
    fn core_warnings_survive_an_empty_mask() {
        assert_eq!(level_of(E_CORE_WARNING, 0), (Level::WARN, "Warning"));
    }

    /// A type no arm recognizes reports at `Warn`, and is still subject to the mask — the
    /// unknown arm is not itself an exemption.
    #[test]
    fn an_unknown_error_type_still_obeys_the_mask() {
        let unknown = 1 << 20; // above E_ALL, so no arm and no reporting bit overlaps it
        assert_eq!(level_of(unknown, unknown), (Level::WARN, "Unknown error"));
        assert_eq!(level_of(unknown, 0), (Level::TRACE, "Unknown error"));
    }

    /// `err_type == 0` has no bit to test against the mask, so without the `err_type != 0`
    /// guard every unknown type would test as masked and report at `Trace`.
    #[test]
    fn a_zero_error_type_reports_unknown_at_warn() {
        assert_eq!(level_of(0, 0), (Level::WARN, "Unknown error"));
    }

    /// The severity boundaries are a contract with `[log] level`: LOG_ERR and below are errors,
    /// LOG_WARNING is the last priority a `warn` filter shows, and LOG_INFO has to land below
    /// `Info` because php-src reports deprecations at that priority.
    #[test]
    fn syslog_severities_keep_their_boundaries() {
        for (priority, want) in [
            (0, Level::ERROR),
            (1, Level::ERROR),
            (2, Level::ERROR),
            (3, Level::ERROR),
            (4, Level::WARN),
            (5, Level::INFO),
            (6, Level::DEBUG),
            (7, Level::DEBUG),
        ] {
            assert_eq!(syslog_to_level(priority), want, "priority {priority}");
        }
    }

    /// The callback takes whatever int php-src passes it; a priority off the scale reports at
    /// `Info` rather than being dropped or raised to an error.
    #[test]
    fn an_out_of_range_syslog_priority_falls_back_to_info() {
        assert_eq!(syslog_to_level(8), Level::INFO);
        assert_eq!(syslog_to_level(-1), Level::INFO);
    }

    /// The agreement the two tables exist together for: both sort a deprecation below `Info`,
    /// so `[log] level = "info"` does not turn deprecations into ordinary traffic.
    #[test]
    fn both_paths_sort_deprecations_below_info() {
        let (level, label) = level_of(E_DEPRECATED, E_DEPRECATED);
        assert_eq!(label, "Deprecated");
        assert!(level > Level::INFO);
        assert!(syslog_to_level(6) > Level::INFO);
    }
}
