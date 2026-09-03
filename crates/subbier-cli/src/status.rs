//! `subbier status` — [`crate::report`]'s rows painted as ANSI text. Colour is
//! never the only carrier: an account that needs attention says so in words
//! too, so `subbier status | cat` loses nothing but the hue.

use libsubby::Snapshot;

use crate::report;
use crate::style::Style;
use crate::{GlobalArgs, Result, runtime};

/// Bar width, in cells — sized for an 80-column terminal.
pub(crate) const BAR_WIDTH: usize = 20;

/// Show usage bars for every account.
#[derive(Debug, Clone, clap::Args)]
pub struct StatusArgs {
    /// Emit the snapshot as JSON instead — the same document `subbier watch
    /// --json` streams.
    #[arg(long)]
    pub json: bool,
}

pub async fn run(global: &GlobalArgs, args: &StatusArgs) -> Result {
    let (snap, local) = runtime::observe(global).await?;

    let out = if args.json {
        // Verbatim: reshaping the document a bar widget consumes would fork it.
        serde_json::to_string_pretty(&snap)?
    } else {
        render_status(&snap, BAR_WIDTH, Style::auto())
    };
    println!("{out}");

    if let Some(local) = local {
        local.shutdown().await;
    }
    Ok(())
}

pub(crate) fn render_status(snap: &Snapshot, width: usize, style: Style) -> String {
    report::build(snap, width)
        .iter()
        .map(|row| style.line(row))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use jiff::{SignedDuration, Timestamp};
    use libsubby::snapshot::{
        ProxyView, RoutingView, ScopedWindow, SnapshotData, SubHealth, SubView, WindowView,
    };
    use libsubby::{CredentialSource, Provider, Severity, SubId};

    use super::*;

    pub(super) const PLAIN: Style = Style::plain();

    fn render(snap: &Snapshot) -> String {
        render_status(snap, BAR_WIDTH, PLAIN)
    }

    fn section<'a>(text: &'a str, header: &str) -> Vec<&'a str> {
        text.lines()
            .skip_while(|l| !l.starts_with(header))
            .skip(1)
            .take_while(|l| !l.starts_with("SUBSCRIPTIONS") && !l.starts_with("PROXIES:"))
            .collect()
    }

    fn window(pct: f32, resets_in: Option<SignedDuration>) -> WindowView {
        WindowView {
            pct,
            resets_at: None,
            resets_in,
            severity: Severity::Ok,
            projection: None,
        }
    }

    fn sub() -> SubView {
        SubView {
            plan_tier: "unknown".into(),
            plan_weight: 1.0,
            id: SubId(1),
            key: libsubby::SubKey::new(Provider::Codex, "acct-1"),
            provider: Provider::Codex,
            label: "anowell@example.com".to_owned(),
            plan: Some("plus".to_owned()),
            source: CredentialSource::Keychain,
            enabled: true,
            health: SubHealth::Ok,
            session: Some(window(42.0, Some(SignedDuration::from_hours(4)))),
            weekly: Some(window(15.0, None)),
            scoped: Vec::new(),
            routing: RoutingView {
                eligible: true,
                active: true,
                ..RoutingView::default()
            },
        }
    }

    fn snapshot(subs: Vec<SubView>) -> Snapshot {
        Snapshot::from(SnapshotData {
            generation: 2,
            subs,
            ..SnapshotData::default()
        })
    }

    fn busy_snapshot(subs: Vec<SubView>) -> Snapshot {
        Snapshot::from(SnapshotData {
            generation: 2,
            subs,
            proxy: ProxyView {
                running: true,
                listening: Some("127.0.0.1:8787".parse().unwrap()),
                proxied_in_flight: 2,
                proxied_requests_total: 17,
                proxied_tokens_1h: 1_200_000,
                ..ProxyView::default()
            },
            ..SnapshotData::default()
        })
    }

    #[test]
    fn zero_subs_renders_advice_rather_than_an_empty_report() {
        let text = render(&snapshot(Vec::new()));
        assert!(text.contains("No accounts yet"), "{text}");
        assert!(text.contains("subbier login codex"), "{text}");
        assert!(text.contains("PROXIES: not running"), "{text}");
    }

    #[test]
    fn proxy_numbers_are_in_the_proxies_section_and_allowance_is_not() {
        let text = render(&busy_snapshot(vec![sub()]));

        let proxies = section(&text, "PROXIES:").join("\n");
        assert!(
            proxies.contains("1.2M tok/1h · 2 in flight · 17 routed"),
            "{proxies}"
        );
        assert!(!proxies.contains('█'), "{proxies}");
        assert!(!proxies.contains('░'), "{proxies}");
        assert!(!proxies.contains('%'), "{proxies}");

        for line in section(&text, "SUBSCRIPTIONS") {
            assert!(!line.contains("tok"), "{line}");
            assert!(!line.contains("in flight"), "{line}");
            assert!(!line.contains("routed"), "{line}");
        }
    }

    #[test]
    fn a_sub_renders_its_allowance_bars_with_aligned_labels() {
        let mut s = sub();
        s.scoped = vec![ScopedWindow {
            name: "fable".to_owned(),
            window: window(3.0, None),
        }];
        let text = render(&snapshot(vec![s]));
        let bars: Vec<&str> = text
            .lines()
            .filter(|l| l.contains('░') || l.contains('█'))
            .collect();
        assert_eq!(bars.len(), 3, "{text}");
        let starts: Vec<usize> = bars.iter().map(|l| l.find(['█', '░']).unwrap()).collect();
        assert!(starts.windows(2).all(|w| w[0] == w[1]), "{bars:?}");
        assert!(text.contains("42%"), "{text}");
    }

    #[test]
    fn the_active_sub_is_marked_and_the_others_are_not() {
        let mut idle = sub();
        idle.id = SubId(2);
        idle.label = "second@example.com".to_owned();
        idle.routing.active = false;
        let text = render(&snapshot(vec![sub(), idle]));
        assert!(text.contains("* codex  anowell@example.com"), "{text}");
        assert!(text.contains("  codex  second@example.com"), "{text}");
    }

    #[test]
    fn a_stale_sub_says_how_old_its_numbers_are_instead_of_hiding_it() {
        let mut s = sub();
        s.health = SubHealth::Stale {
            since: Timestamp::now() - SignedDuration::from_mins(7),
            error: "usage timeout".to_owned(),
        };
        let snap = Snapshot::from(SnapshotData {
            generation: 2,
            captured_at: Timestamp::now(),
            subs: vec![s],
            ..SnapshotData::default()
        });
        let text = render(&snap);
        assert!(text.contains("stale for 7m"), "{text}");
        assert!(text.contains("usage timeout"), "{text}");
        // Stale, not absent: the previous numbers are still shown.
        assert!(text.contains("42%"), "{text}");
    }

    #[test]
    fn a_sub_with_no_windows_says_so_rather_than_drawing_an_empty_bar() {
        let mut s = sub();
        s.session = None;
        s.weekly = None;
        s.health = SubHealth::Unknown;
        let text = render(&snapshot(vec![s]));
        assert!(text.contains("no allowance figures yet"), "{text}");
        assert!(text.contains("never polled"), "{text}");
        // A 0% bar would read as "nothing used", which we do not know.
        assert!(!text.contains('░'), "{text}");
    }

    #[test]
    fn a_warning_account_says_so_in_words_and_not_only_in_colour() {
        let mut s = sub();
        s.session = Some(WindowView {
            severity: Severity::Critical,
            ..window(94.0, None)
        });
        let text = render(&snapshot(vec![s]));
        assert!(text.contains("— critical"), "{text}");
    }

    /// The URLs are `subbier env`'s job, in the form you can paste.
    #[test]
    fn the_report_carries_no_base_urls() {
        let text = render(&busy_snapshot(vec![sub()]));
        assert!(!text.contains("http://"), "{text}");
    }

    #[test]
    fn only_surprising_settings_reach_the_header() {
        let plain = render(&busy_snapshot(Vec::new()));
        let header = plain.lines().find(|l| l.starts_with("PROXIES:")).unwrap();
        assert_eq!(header, "PROXIES: round-robin on port 8787", "{header}");

        let mut data = SnapshotData {
            generation: 2,
            proxy: ProxyView {
                running: true,
                listening: Some("127.0.0.1:8787".parse().unwrap()),
                requires_key: true,
                ..ProxyView::default()
            },
            ..SnapshotData::default()
        };
        data.settings.auto_switch = false;
        data.settings.sticky = true; // round-robin is per-request by default
        data.settings.providers_proxied = [true, false];
        let text = render(&Snapshot::from(data));
        let header = text.lines().find(|l| l.starts_with("PROXIES:")).unwrap();
        assert!(header.contains("· sticky"), "{header}");
        assert!(header.contains("auto-switch off"), "{header}");
        assert!(header.contains("only"), "{header}");
        assert!(header.contains("key required"), "{header}");
    }
}

#[cfg(test)]
mod pool_status_tests {
    use jiff::SignedDuration;
    use libsubby::snapshot::{
        PoolView, ProxyView, RoutingView, SnapshotData, SubHealth, SubView, WindowView,
    };
    use libsubby::{CredentialSource, Provider, Severity, SubId};

    use super::tests::PLAIN;
    use super::*;

    fn pool(name: &str, members: &[u32], eligible: &[u32]) -> PoolView {
        PoolView {
            name: name.to_owned(),
            provider: None,
            members: members.iter().map(|&i| SubId(i)).collect(),
            eligible: eligible.iter().map(|&i| SubId(i)).collect(),
            max_session_pct: 100.0,
            max_weekly_pct: 100.0,
            openai_base_url: Some(format!("http://127.0.0.1:8787/pool/{name}/v1")),
            anthropic_base_url: Some(format!("http://127.0.0.1:8787/pool/{name}")),
            proxied_in_flight: 0,
            proxied_tokens_1h: 0,
        }
    }

    fn member(id: u32, weekly_pct: f32) -> SubView {
        SubView {
            plan_tier: "unknown".into(),
            plan_weight: 1.0,
            id: SubId(id),
            key: libsubby::SubKey::new(Provider::Codex, format!("acct-{id}")),
            provider: Provider::Codex,
            label: format!("user{id}@example.com"),
            plan: Some("plus".to_owned()),
            source: CredentialSource::Keychain,
            enabled: true,
            health: SubHealth::Ok,
            session: Some(WindowView {
                pct: 1.0,
                resets_at: None,
                resets_in: Some(SignedDuration::from_hours(2)),
                severity: Severity::Ok,
                projection: None,
            }),
            weekly: Some(WindowView {
                pct: weekly_pct,
                resets_at: None,
                resets_in: None,
                severity: Severity::Ok,
                projection: None,
            }),
            scoped: Vec::new(),
            routing: RoutingView {
                eligible: true,
                ..RoutingView::default()
            },
        }
    }

    fn snapshot(pools: Vec<PoolView>, subs: Vec<SubView>) -> Snapshot {
        SnapshotData {
            generation: 2,
            pools,
            subs,
            proxy: ProxyView {
                running: true,
                listening: Some("127.0.0.1:8787".parse().unwrap()),
                ..ProxyView::default()
            },
            ..SnapshotData::default()
        }
        .into()
    }

    fn proxies(snap: &Snapshot) -> String {
        let text = render_status(snap, BAR_WIDTH, PLAIN);
        text[text.find("PROXIES:").expect("a proxies header")..]
            .trim_end()
            .to_owned()
    }

    #[test]
    fn no_pools_still_lists_the_default_endpoint() {
        let out = proxies(&snapshot(Vec::new(), Vec::new()));
        assert!(out.contains("\n  default\n"), "{out}");
        assert!(!out.contains("/pool/"), "{out}");
    }

    #[test]
    fn a_pool_lists_its_members_rather_than_counting_them() {
        let out = proxies(&snapshot(
            vec![pool("moonshot", &[1, 2], &[1, 2])],
            vec![member(1, 5.0), member(2, 5.0)],
        ));
        assert!(out.contains("\n  moonshot"), "{out}");
        assert!(out.contains("    codex  user1@example.com\n"), "{out}");
        assert!(out.contains("    codex  user2@example.com"), "{out}");
        assert!(!out.contains("inactive"), "{out}");
        // A pool endpoint keeps no lifetime total.
        assert!(out.contains("    0 tok/1h · 0 in flight\n"), "{out}");
    }

    #[test]
    fn a_member_held_back_by_a_ceiling_says_which_and_why() {
        let held = PoolView {
            max_weekly_pct: 50.0,
            ..pool("moonshot", &[1, 2], &[2])
        };
        let out = proxies(&snapshot(vec![held], vec![member(1, 80.0), member(2, 5.0)]));
        assert!(
            out.contains("codex  user1@example.com (inactive - exceeds 50% weekly usage)"),
            "{out}"
        );
        assert!(out.ends_with("codex  user2@example.com"), "{out}");
        assert!(out.contains("under 50% weekly"), "{out}");

        let unset = proxies(&snapshot(
            vec![pool("critical", &[1], &[1])],
            vec![member(1, 5.0)],
        ));
        assert!(!unset.contains("under"), "{unset}");
    }

    /// An account that is simply logged out is not a ceiling problem.
    #[test]
    fn an_unusable_member_reports_the_account_problem_not_the_ceiling() {
        let held = PoolView {
            max_weekly_pct: 50.0,
            ..pool("moonshot", &[1], &[])
        };
        let mut logged_out = member(1, 80.0);
        logged_out.routing.eligible = false;
        logged_out.health = SubHealth::NeedsLogin {
            error: "refresh failed".to_owned(),
        };
        let out = proxies(&snapshot(vec![held], vec![logged_out]));
        assert!(out.contains("(inactive - needs login)"), "{out}");
        assert!(!out.contains("weekly usage"), "{out}");
    }

    /// An endpoint is a live listener; a column of zeros would look like traffic.
    #[test]
    fn a_stopped_proxy_lists_no_endpoints() {
        let snap: Snapshot = SnapshotData {
            generation: 2,
            pools: vec![pool("moonshot", &[1], &[1])],
            subs: vec![member(1, 5.0)],
            ..SnapshotData::default()
        }
        .into();
        let out = proxies(&snap);
        assert!(out.starts_with("PROXIES: not running"), "{out}");
        assert!(!out.contains("moonshot"), "{out}");
        assert!(!out.contains("default"), "{out}");
    }
}
