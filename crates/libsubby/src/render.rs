//! Shared render helpers: bars, percentages, durations, token counts. Width
//! always comes from the caller and nothing here reads a clock. Every function
//! returns one field, never a composed row: padding and colour depend on things
//! `libsubby` must not know.

use jiff::SignedDuration;

const BAR_FILLED: char = '█';
const BAR_EMPTY: char = '░';

/// A meter bar of exactly `width` cells. `pct` is percentage points, clamped; a
/// non-finite value is unknown and renders empty rather than alarming.
#[must_use]
pub fn bar(pct: f32, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let clamped = if pct.is_finite() {
        pct.clamp(0.0, 100.0)
    } else {
        0.0
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let filled = ((clamped / 100.0) * width as f32).round() as usize;
    let filled = filled.min(width);
    let mut s = String::with_capacity(width * BAR_FILLED.len_utf8());
    for _ in 0..filled {
        s.push(BAR_FILLED);
    }
    for _ in filled..width {
        s.push(BAR_EMPTY);
    }
    s
}

/// A percentage-point value as a whole-number string; non-finite renders `"0%"`.
#[must_use]
pub fn pct(v: f32) -> String {
    #[allow(clippy::cast_possible_truncation)]
    let n = if v.is_finite() { v.round() as i32 } else { 0 };
    format!("{n}%")
}

/// A duration as `"5d 22h"` / `"4h 41m"` / `"3m"` / `"now"`.
///
/// Truncates rather than rounding so it never overstates the time remaining,
/// except sub-minute, which floors at `"1m"` — `"0m"` reads as "already done".
/// A past instant is `"now"`, not a negative.
#[must_use]
pub fn duration(d: SignedDuration) -> String {
    let millis = d.as_millis();
    if millis <= 0 {
        return "now".to_owned();
    }
    let minutes = millis / 60_000;
    let hours = minutes / 60;
    let days = hours / 24;
    if days > 0 {
        format!("{days}d {}h", hours % 24)
    } else if hours > 0 {
        format!("{hours}h {}m", minutes % 60)
    } else {
        format!("{}m", minutes.max(1))
    }
}

/// [`duration`] to a single unit, for a fixed-width column.
///
/// ```
/// # use jiff::SignedDuration;
/// # use libsubby::render::duration_short;
/// assert_eq!(duration_short(SignedDuration::from_mins(281)), "4h");
/// assert_eq!(duration_short(SignedDuration::from_hours(74)), "3d");
/// assert_eq!(duration_short(SignedDuration::from_secs(30)), "1m");
/// ```
#[must_use]
pub fn duration_short(d: SignedDuration) -> String {
    let millis = d.as_millis();
    if millis <= 0 {
        return "now".to_owned();
    }
    let minutes = millis / 60_000;
    let hours = minutes / 60;
    let days = hours / 24;
    if days > 0 {
        format!("{days}d")
    } else if hours > 0 {
        format!("{hours}h")
    } else {
        format!("{}m", minutes.max(1))
    }
}

/// A token count as `"305.2M"` / `"4.5K"` / `"0"`.
#[must_use]
pub fn tokens(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let f = n as f64;
    if n >= 1_000_000_000 {
        format!("{:.1}B", f / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1}M", f / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}K", f / 1e3)
    } else {
        n.to_string()
    }
}

/// One row of proxy-observed metrics:
/// `"1.2M tok/1h · 2 in flight · 17 routed"`.
///
/// `routed` is the endpoint's lifetime request total, `None` for an endpoint
/// that keeps none. The caller must render this under something naming the
/// endpoint: next to an allowance bar it reads as a caption for that bar, which
/// is the allowance/proxy-observed confusion [`crate::snapshot::RoutingView`]
/// exists to prevent.
#[must_use]
pub fn proxy_metrics(in_flight: u32, tokens_1h: u64, routed: Option<u64>) -> String {
    let mut row = format!("{} tok/1h · {in_flight} in flight", tokens(tokens_1h));
    if let Some(routed) = routed {
        row.push_str(&format!(" · {routed} routed"));
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bar_is_proportional_and_always_exactly_width_cells() {
        assert_eq!(bar(0.0, 20), "░".repeat(20));
        assert_eq!(bar(100.0, 20), "█".repeat(20));
        assert_eq!(
            bar(50.0, 20),
            format!("{}{}", "█".repeat(10), "░".repeat(10))
        );
        for width in [0usize, 1, 3, 7, 20, 64] {
            for pct in [-5.0f32, 0.0, 0.4, 33.3, 50.0, 99.6, 100.0, 150.0] {
                assert_eq!(
                    bar(pct, width).chars().count(),
                    width,
                    "bar({pct}, {width})"
                );
            }
        }
    }

    #[test]
    fn an_unknown_or_out_of_range_percentage_never_alarms() {
        assert_eq!(bar(-5.0, 20), "░".repeat(20));
        assert_eq!(bar(150.0, 20), "█".repeat(20));
        for unknown in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(bar(unknown, 20), "░".repeat(20), "{unknown}");
            assert_eq!(pct(unknown), "0%", "{unknown}");
        }
        assert_eq!(pct(42.1), "42%");
        assert_eq!(pct(42.6), "43%");
    }

    #[test]
    fn a_duration_truncates_and_never_overstates_the_time_left() {
        assert_eq!(duration(SignedDuration::ZERO), "now");
        assert_eq!(duration(SignedDuration::from_hours(-3)), "now");
        assert_eq!(duration(SignedDuration::from_secs(30)), "1m");
        assert_eq!(duration(SignedDuration::from_mins(45)), "45m");
        assert_eq!(duration(SignedDuration::from_hours(5 * 24 + 22)), "5d 22h");
        assert_eq!(
            duration(SignedDuration::from_hours(24) + SignedDuration::from_mins(59)),
            "1d 0h"
        );
        assert_eq!(
            duration(SignedDuration::from_hours(1) + SignedDuration::from_secs(59)),
            "1h 0m"
        );
    }

    #[test]
    fn a_token_count_scales_to_k_m_b() {
        assert_eq!(tokens(999), "999");
        assert_eq!(tokens(1_500), "1.5K");
        assert_eq!(tokens(2_500_000), "2.5M");
        assert_eq!(tokens(3_200_000_000), "3.2B");
    }

    #[test]
    fn proxy_metrics_omits_a_total_the_endpoint_does_not_keep() {
        assert_eq!(
            proxy_metrics(2, 1_200_000, Some(17)),
            "1.2M tok/1h · 2 in flight · 17 routed"
        );
        assert_eq!(proxy_metrics(0, 0, None), "0 tok/1h · 0 in flight");
    }
}
