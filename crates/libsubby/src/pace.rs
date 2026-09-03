//! Burn-rate projection: a straight-line extrapolation of a window's consumed
//! percentage over `[started_at, now]` out to 100%. Every function takes `now`.
//! A least-squares fit over token history is deliberately absent: a
//! percentage-based subscription window has no token quota to fit against.

use jiff::{SignedDuration, Timestamp};

use crate::model::{Projection, UsageWindow};

/// Linearly projects when `window` will hit 100%, as of `now`.
///
/// `None` unless `0 < pct < 100`, both bounds are known (`started_at` is
/// derived upstream and never guessed), `started_at < now < resets_at`, and the
/// projection lands strictly before `resets_at` — a window that resets before it
/// could be exhausted is noise, not a warning.
#[must_use]
pub fn project(window: &UsageWindow, now: Timestamp) -> Option<Projection> {
    let pct = window.pct;
    if !pct.is_finite() || pct <= 0.0 || pct >= 100.0 {
        return None;
    }

    let started_at = window.started_at?;
    let resets_at = window.resets_at?;
    if started_at >= now || resets_at <= now || resets_at <= started_at {
        return None;
    }

    let elapsed = started_at.duration_until(now);
    // f64: a 7-day window at 0.1% loses the projection to f32 slop.
    let remaining = f64::from(100.0 - pct);
    let until_exhaustion =
        SignedDuration::try_from_secs_f64(elapsed.as_secs_f64() * remaining / f64::from(pct))
            .ok()?;
    let exhausts_at = now.checked_add(until_exhaustion).ok()?;

    if exhausts_at >= resets_at {
        return None;
    }

    Some(Projection {
        exhausts_at,
        until_exhaustion,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("test timestamp")
    }

    fn now() -> Timestamp {
        ts("2026-08-23T12:00:00Z")
    }

    fn window(pct: f32, started_at: &str, resets_at: &str) -> UsageWindow {
        UsageWindow {
            pct,
            resets_at: Some(ts(resets_at)),
            started_at: Some(ts(started_at)),
        }
    }

    #[test]
    fn projects_linearly_from_started_at() {
        let w = window(50.0, "2026-08-23T10:00:00Z", "2026-08-23T16:00:00Z");
        let p = project(&w, now()).expect("actionable projection");
        assert_eq!(p.until_exhaustion, SignedDuration::from_hours(2));
        assert_eq!(p.exhausts_at, ts("2026-08-23T14:00:00Z"));

        let w = window(25.0, "2026-08-23T11:00:00Z", "2026-08-23T16:00:00Z");
        let p = project(&w, now()).expect("actionable projection");
        assert_eq!(p.exhausts_at, ts("2026-08-23T15:00:00Z"));
    }

    #[test]
    fn withholds_a_projection_that_does_not_land_before_the_reset() {
        let late = window(10.0, "2026-08-23T10:00:00Z", "2026-08-23T16:00:00Z");
        assert_eq!(project(&late, now()), None);
        let exact = window(50.0, "2026-08-23T10:00:00Z", "2026-08-23T14:00:00Z");
        assert_eq!(project(&exact, now()), None);
    }

    #[test]
    fn refuses_percentages_outside_the_open_range() {
        for pct in [0.0, -5.0, 100.0, 150.0, f32::NAN, f32::INFINITY] {
            let w = window(pct, "2026-08-23T10:00:00Z", "2026-08-23T16:00:00Z");
            assert_eq!(project(&w, now()), None, "pct {pct} should not project");
        }
    }

    #[test]
    fn refuses_an_unknown_start_or_reset_rather_than_guessing_one() {
        let no_start = UsageWindow {
            pct: 50.0,
            resets_at: Some(ts("2026-08-23T16:00:00Z")),
            started_at: None,
        };
        assert_eq!(project(&no_start, now()), None);
        assert_eq!(project(&UsageWindow::from_pct(50.0), now()), None);
    }

    #[test]
    fn refuses_a_stale_or_inverted_window() {
        let cases = [
            // reset passed, reset at now, start in future, start at now, inverted
            ("2026-08-23T06:00:00Z", "2026-08-23T11:59:00Z"),
            ("2026-08-23T06:00:00Z", "2026-08-23T12:00:00Z"),
            ("2026-08-23T13:00:00Z", "2026-08-23T18:00:00Z"),
            ("2026-08-23T12:00:00Z", "2026-08-23T18:00:00Z"),
            ("2026-08-23T11:00:00Z", "2026-08-23T10:00:00Z"),
        ];
        for (started_at, resets_at) in cases {
            let w = window(50.0, started_at, resets_at);
            assert_eq!(project(&w, now()), None, "{started_at}..{resets_at}");
        }
    }
}
