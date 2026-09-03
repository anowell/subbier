//! Severity bands and notification transitions.
//!
//! Percentages are percentage points on `0..=100`, not fractions.

use crate::model::Severity;

/// Default `ui.warn-pct`, in percentage points.
pub const DEFAULT_WARN_PCT: f32 = 75.0;

/// Default `ui.critical-pct`, in percentage points.
pub const DEFAULT_CRITICAL_PCT: f32 = 90.0;

/// Classifies a percentage-point value; both thresholds are inclusive lower
/// bounds.
///
/// A misconfigured `warn >= critical` collapses the warn band down onto
/// `critical`. A non-finite or negative percent means "unknown" and classifies
/// [`Severity::Ok`] rather than raising an alarm; a non-finite threshold falls
/// back to its default.
#[must_use]
pub fn severity_for(pct: f32, warn: f32, critical: f32) -> Severity {
    let critical = if critical.is_finite() {
        critical
    } else {
        DEFAULT_CRITICAL_PCT
    };
    let mut warn = if warn.is_finite() {
        warn
    } else {
        DEFAULT_WARN_PCT
    };
    if warn >= critical {
        warn = critical;
    }

    if !pct.is_finite() || pct < 0.0 {
        return Severity::Ok;
    }
    if pct >= critical {
        Severity::Critical
    } else if pct >= warn {
        Severity::Warn
    } else {
        Severity::Ok
    }
}

/// The state after a refresh, plus the one notification (if any) it crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    /// Store this as the next call's `prev`, always — see
    /// [`notification_transition`].
    pub severity: Severity,
    pub notify: Option<Severity>,
}

/// Decides whether a refresh should fire a notification. Only strictly-upward
/// band changes notify; a `prev` of `None` never does, having witnessed no
/// crossing.
///
/// The caller must store [`Transition::severity`] even when `notify` is `None`:
/// that is what re-arms the next crossing after a drop.
#[must_use]
pub fn notification_transition(prev: Option<Severity>, current: Severity) -> Transition {
    Transition {
        severity: current,
        notify: match prev {
            Some(previous) if current > previous => Some(current),
            _ => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_are_inclusive_lower_bounds() {
        assert_eq!(severity_for(59.0, 60.0, 80.0), Severity::Ok);
        assert_eq!(severity_for(60.0, 60.0, 80.0), Severity::Warn);
        assert_eq!(severity_for(79.0, 60.0, 80.0), Severity::Warn);
        assert_eq!(severity_for(80.0, 60.0, 80.0), Severity::Critical);
    }

    #[test]
    fn misconfigured_warn_collapses_the_band_rather_than_contradicting() {
        for pct in [70.0, 79.0] {
            assert_eq!(severity_for(pct, 90.0, 80.0), Severity::Ok, "{pct}");
        }
        assert_eq!(severity_for(80.0, 90.0, 80.0), Severity::Critical);
    }

    #[test]
    fn an_unknown_percentage_is_never_alarming() {
        for pct in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0] {
            assert_eq!(
                severity_for(pct, DEFAULT_WARN_PCT, DEFAULT_CRITICAL_PCT),
                Severity::Ok,
                "{pct}"
            );
        }
        assert_eq!(severity_for(-1.0, -10.0, -5.0), Severity::Ok);
    }

    #[test]
    fn non_finite_thresholds_fall_back_to_the_defaults() {
        let between = (DEFAULT_WARN_PCT + DEFAULT_CRITICAL_PCT) / 2.0;
        assert_eq!(severity_for(between, f32::NAN, f32::NAN), Severity::Warn);
        assert_eq!(
            severity_for(between, f32::NAN, f32::INFINITY),
            Severity::Warn
        );
    }

    #[test]
    fn a_refresh_loop_emits_once_per_upward_crossing() {
        assert_eq!(
            notification_transition(None, Severity::Critical).notify,
            None
        );
        assert_eq!(
            notification_transition(Some(Severity::Ok), Severity::Critical).notify,
            Some(Severity::Critical)
        );

        let pcts = [10.0, 50.0, 76.0, 80.0, 89.0, 95.0, 99.0, 40.0, 92.0];
        let mut prev = None;
        let mut fired = Vec::new();
        for pct in pcts {
            let t = notification_transition(
                prev,
                severity_for(pct, DEFAULT_WARN_PCT, DEFAULT_CRITICAL_PCT),
            );
            if let Some(n) = t.notify {
                fired.push(n);
            }
            prev = Some(t.severity);
        }
        assert_eq!(
            fired,
            vec![Severity::Warn, Severity::Critical, Severity::Critical]
        );
    }
}
