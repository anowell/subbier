//! One sub, one row — **as data**; the drawing is [`crate::menu::rowview`]'s. Nothing here
//! needs AppKit.

use libsubby::snapshot::{SubHealth, SubView, WindowView};
use libsubby::{Severity, plan, render};

#[derive(Clone, PartialEq, Debug)]
pub struct Meter {
    pub pct: f32,
    pub severity: Severity,
    /// `"(2h)"`, `"(3d)"`, `"(now)"` — empty when the provider stated no reset.
    pub reset: String,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Row {
    /// The `SESSION` / `WEEKLY` headers. One per tab, at the top.
    Columns,
    /// A provider's rule: `Claude ──────────────────────────────`.
    Section {
        name: String,
    },
    Sub(Box<SubRow>),
}

#[derive(Clone, PartialEq, Debug)]
pub struct SubRow {
    pub label: String,
    /// Resolved by [`plan::display_name`], so the column cannot disagree with the weight
    /// that tier gave the menu bar's one percentage.
    pub plan: String,
    /// Why this account cannot be used. Drawn **in place of** the plan, in the warning
    /// colour: a plan will keep until tomorrow and this will not.
    pub flag: Option<String>,
    /// Whether the proxy is routing here right now — what the gutter dot means.
    pub active: bool,
    /// The 5h window. `None` draws **no bar at all**: an empty track reads as a real 0%.
    pub session: Option<Meter>,
    /// The 7d window.
    pub weekly: Option<Meter>,
}

impl Row {
    /// An `NSMenuItem` takes its height from its view's frame.
    #[must_use]
    pub const fn height(&self) -> f64 {
        match self {
            Self::Columns => 18.0,
            Self::Section { .. } => 26.0,
            Self::Sub(_) => 24.0,
        }
    }
}

#[must_use]
pub fn row_of(sub: &SubView) -> Row {
    Row::Sub(Box::new(SubRow {
        label: sub.label.clone(),
        plan: plan::display_name(sub.provider, &sub.plan_tier).to_owned(),
        flag: flag_of(sub),
        active: sub.routing.active,
        session: meter(sub.session.as_ref()),
        weekly: meter(sub.weekly.as_ref()),
    }))
}

/// A name and a hairline, and no proxy metrics — summing an endpoint's traffic across a
/// group is how a strict subset starts reading as a total. Those live on the `Proxy ▸` row.
#[must_use]
pub fn section_of(name: &str) -> Row {
    Row::Section {
        name: name.to_owned(),
    }
}

/// Why this account is not usable, in two words or fewer.
fn flag_of(sub: &SubView) -> Option<String> {
    if !sub.enabled {
        return Some("off".to_owned());
    }
    match sub.health {
        SubHealth::NeedsLogin { .. } => Some("needs login".to_owned()),
        SubHealth::Exhausted { .. } => Some("exhausted".to_owned()),
        SubHealth::Stale { .. } => Some("stale".to_owned()),
        SubHealth::Ok | SubHealth::Unknown => None,
    }
}

fn meter(window: Option<&WindowView>) -> Option<Meter> {
    let window = window?;
    Some(Meter {
        pct: window.pct,
        severity: window.severity,
        reset: reset_text(window),
    })
}

fn reset_text(window: &WindowView) -> String {
    match window.resets_in {
        // Past its reset but not re-polled: "(0m)" would be a lie in a way "(now)" is not.
        Some(d) if d.as_millis() <= 0 => "(now)".to_owned(),
        Some(d) => format!("({})", render::duration_short(d)),
        None => String::new(),
    }
}

/// Everything the row cannot fit, on hover. **Allowance only**, like the row itself: a
/// tooltip is exactly where a strict subset starts reading as the total above it.
#[must_use]
pub fn tooltip(sub: &SubView) -> String {
    // What the vendor actually said: the only place to compare it with the resolved tier.
    let tier = plan::display_name(sub.provider, &sub.plan_tier);
    let mut lines = vec![match &sub.plan {
        Some(plan) if plan != tier => format!(
            "{} — {} · {tier} ({plan})",
            sub.label,
            sub.provider.display_name()
        ),
        _ => format!("{} — {} · {tier}", sub.label, sub.provider.display_name()),
    }];

    // A health problem replaces the countdowns: "resets in 4h 41m" says nothing useful
    // about an account whose refresh token is dead.
    match &sub.health {
        SubHealth::NeedsLogin { error } => lines.push(format!("Needs login — {error}")),
        SubHealth::Exhausted { .. } => {
            lines.push("Exhausted — skipped until the allowance resets".to_owned());
        }
        SubHealth::Stale { error, .. } => lines.push(format!("Stale — {error}")),
        SubHealth::Ok | SubHealth::Unknown => {
            lines.push(window_line("Session", sub.session.as_ref()));
            lines.push(window_line("Weekly", sub.weekly.as_ref()));
        }
    }

    if !sub.enabled {
        lines.push("Off — `enabled #false` in config.kdl".to_owned());
    }
    if sub.routing.active {
        lines.push("The account the proxy is routing to right now".to_owned());
    }
    lines.join("\n")
}

fn window_line(label: &str, window: Option<&WindowView>) -> String {
    let Some(window) = window else {
        return format!("{label:<7}    —  ·  not reported");
    };
    let reset = match window.resets_in {
        // `duration` renders an elapsed reset as "now", and "resets in now" is not English.
        Some(d) if d.as_millis() <= 0 => "resets now".to_owned(),
        Some(d) => format!("resets in {}", render::duration(d)),
        None => "reset time unknown".to_owned(),
    };
    format!("{label:<7} {:>4}  ·  {reset}", render::pct(window.pct))
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::{SignedDuration, Timestamp};
    use libsubby::snapshot::RoutingView;
    use libsubby::{CredentialSource, Provider, SubId};

    fn window(pct: f32, severity: Severity, resets_in: Option<SignedDuration>) -> WindowView {
        WindowView {
            pct,
            resets_at: resets_in.map(|d| Timestamp::now() + d),
            resets_in,
            severity,
            projection: None,
        }
    }

    fn sub() -> SubView {
        SubView {
            plan_tier: "max-20x".into(),
            plan_weight: 20.0,
            id: SubId(1),
            key: libsubby::SubKey::new(Provider::Claude, "acct-1"),
            provider: Provider::Claude,
            label: "anthony@howie.ai".to_owned(),
            plan: Some("max20".to_owned()),
            source: CredentialSource::Keychain,
            enabled: true,
            health: SubHealth::Ok,
            session: Some(window(
                67.0,
                Severity::Ok,
                Some(SignedDuration::from_mins(281)),
            )),
            weekly: Some(window(31.0, Severity::Ok, None)),
            scoped: Vec::new(),
            routing: RoutingView {
                eligible: true,
                active: true,
                ..RoutingView::default()
            },
        }
    }

    fn sub_row(sub: &SubView) -> SubRow {
        match row_of(sub) {
            Row::Sub(row) => *row,
            other => panic!("an account is a Sub row: {other:?}"),
        }
    }

    #[test]
    fn the_row_names_the_plan_it_is_weighted_by() {
        assert_eq!(sub_row(&sub()).plan, "Max 20x");

        let mut codex = sub();
        codex.provider = Provider::Codex;
        codex.plan_tier = "plus".into();
        assert_eq!(sub_row(&codex).plan, "Plus");

        // Never blank: an id this build does not know still names something.
        codex.plan_tier = "not-a-tier".into();
        assert_eq!(sub_row(&codex).plan, "Unknown");
    }

    #[test]
    fn an_unusable_account_says_why_in_words() {
        let mut dead = sub();
        dead.health = SubHealth::NeedsLogin {
            error: "token expired".to_owned(),
        };
        assert_eq!(sub_row(&dead).flag.as_deref(), Some("needs login"));

        let mut off = sub();
        off.enabled = false;
        assert_eq!(sub_row(&off).flag.as_deref(), Some("off"));

        // A healthy account carries no flag at all, so the column is its plan.
        assert_eq!(sub_row(&sub()).flag, None);
    }

    #[test]
    fn an_unreported_window_is_absent_not_zero() {
        let mut bare = sub();
        bare.session = None;
        let row = sub_row(&bare);
        assert!(row.session.is_none());
        assert!(row.weekly.is_some());
    }

    #[test]
    fn a_meter_carries_its_band_and_its_countdown() {
        let mut s = sub();
        s.session = Some(window(
            95.0,
            Severity::Critical,
            Some(SignedDuration::from_hours(2)),
        ));
        let row = sub_row(&s);
        let session = row.session.expect("reported");
        assert_eq!(session.severity, Severity::Critical);
        assert_eq!(session.reset, "(2h)");
        // No reset stated: no countdown invented.
        assert_eq!(row.weekly.expect("reported").reset, "");

        s.session = Some(window(
            10.0,
            Severity::Ok,
            Some(SignedDuration::from_mins(-5)),
        ));
        assert_eq!(sub_row(&s).session.expect("reported").reset, "(now)");
    }

    #[test]
    fn the_tooltip_carries_the_countdowns_that_do_not_fit() {
        let tip = tooltip(&sub());
        assert!(tip.contains("resets in 4h 41m"), "{tip}");
        assert!(tip.contains("reset time unknown"), "{tip}");
        assert!(
            tip.starts_with("anthony@howie.ai — Claude · Max 20x (max20)"),
            "{tip}"
        );
    }

    #[test]
    fn the_tooltip_says_nothing_about_what_the_proxy_carried() {
        let tip = tooltip(&sub());
        assert!(!tip.contains("tok/1h"), "{tip}");
        assert!(!tip.contains("in flight"), "{tip}");
        assert!(!tip.contains("via proxy"), "{tip}");
    }

    #[test]
    fn a_health_problem_replaces_the_countdowns() {
        let mut dead = sub();
        dead.health = SubHealth::NeedsLogin {
            error: "token expired".to_owned(),
        };
        let tip = tooltip(&dead);
        assert!(tip.contains("Needs login — token expired"), "{tip}");
        assert!(!tip.contains("resets in"), "{tip}");
    }
}
