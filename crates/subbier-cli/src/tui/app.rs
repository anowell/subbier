//! The TUI's state, and every key that changes it. Free of ratatui and of I/O,
//! so the whole interaction model is testable without a terminal;
//! [`super::draw`] reads this and never writes it.

use std::collections::BTreeSet;

use jiff::{SignedDuration, Timestamp};
use libsubby::history::{Endpoint, Metric, Rate};
use libsubby::snapshot::SubView;
use libsubby::{Snapshot, SubId, WindowKind};

/// A span, and the bucket its rows are summed into: roughly sixty samples, finer
/// than any pane is wide, so a chart resamples *down* whatever the terminal size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window {
    pub(crate) span: SignedDuration,
    pub(crate) bucket: SignedDuration,
    pub(crate) label: &'static str,
    /// A property of the span; allowance charts ignore it, being percentages.
    pub(crate) rate: Rate,
}

const fn window(hours: i64, bucket_mins: i64, label: &'static str, rate: Rate) -> Window {
    Window {
        span: SignedDuration::from_hours(hours),
        bucket: SignedDuration::from_mins(bucket_mins),
        label,
        rate,
    }
}

const H1: Window = window(1, 1, "1h", Rate::PerMinute);
const H6: Window = window(6, 5, "6h", Rate::PerMinute);
/// Not on the [`Range`] ladder: the session window is 5h, and 8h is the shortest
/// span always containing a whole one plus the shape of the one before it.
const H8: Window = window(8, 8, "8h", Rate::PerMinute);
const H12: Window = window(12, 10, "12h", Rate::PerMinute);
const D1: Window = window(24, 20, "24h", Rate::PerMinute);
const D7: Window = window(24 * 7, 120, "7d", Rate::PerHour);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Chart {
    /// One allowance window, every shown account on it: account-wide.
    Allowance(WindowKind),
    /// One proxy-observed rate, for the endpoint `p` has selected.
    Throughput(Metric),
}

impl Chart {
    /// `"SESSION"`, `"TOK/MIN"`: a throughput chart is named for its unit, so a
    /// denominator that follows the span cannot change silently.
    pub(crate) fn title(&self, window: Window) -> String {
        match self {
            Chart::Allowance(kind) => kind.as_str().to_uppercase(),
            Chart::Throughput(metric) => metric.unit(window.rate).to_uppercase(),
        }
    }

    /// Under [`Range::Mixed`]: the span over which *this* series says something.
    fn natural(&self) -> Window {
        match self {
            Chart::Allowance(WindowKind::Session) => H8,
            Chart::Allowance(_) => D7,
            Chart::Throughput(_) => H1,
        }
    }
}

/// How far back the charts look. [`Range::Mixed`], the default, is each chart
/// over the span that chart is about ([`Chart::natural`]); the fixed stops
/// answer what Mixed cannot: all of them at the same moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Range {
    Mixed,
    H1,
    H6,
    H12,
    D1,
    D7,
}

impl Range {
    /// In key order: `0` is Mixed, `1`–`5` the ladder.
    pub(crate) const ALL: [Range; 6] = [
        Range::Mixed,
        Range::H1,
        Range::H6,
        Range::H12,
        Range::D1,
        Range::D7,
    ];

    pub(crate) fn window(self, chart: &Chart) -> Window {
        match self {
            Range::Mixed => chart.natural(),
            Range::H1 => H1,
            Range::H6 => H6,
            Range::H12 => H12,
            Range::D1 => D1,
            Range::D7 => D7,
        }
    }

    /// Allowance rows come back **unbucketed**, so one read serves every chart.
    pub(crate) fn allowance_span(self) -> SignedDuration {
        [
            Chart::Allowance(WindowKind::Session),
            Chart::Allowance(WindowKind::Weekly),
        ]
        .iter()
        .map(|chart| self.window(chart).span)
        .max()
        .unwrap_or(D7.span)
    }

    /// Unlike allowance, throughput is summed in sqlite: the query needs a bucket.
    pub(crate) fn throughput_window(self) -> Window {
        self.window(&Chart::Throughput(Metric::Tokens))
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Range::Mixed => "mixed",
            Range::H1 => H1.label,
            Range::H6 => H6.label,
            Range::H12 => H12.label,
            Range::D1 => D1.label,
            Range::D7 => D7.label,
        }
    }
}

/// `Both` is the default and degrades to `Subs` on a short terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Both,
    Subs,
    Proxy,
}

impl Page {
    const CYCLE: [Page; 3] = [Page::Both, Page::Subs, Page::Proxy];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Page::Both => "all",
            Page::Subs => "subs",
            Page::Proxy => "proxy",
        }
    }
}

pub(crate) struct App {
    pub(crate) snap: Snapshot,
    pub(crate) allowance: libsubby::history::AllowanceHistory,
    pub(crate) throughput: libsubby::history::Throughput,
    /// Index into [`App::subs`] — the account `space` would toggle.
    cursor: usize,
    /// Stored as **exclusions**, so an account that logs in mid-session shows up
    /// on the charts without the user having to go and find it.
    hidden: BTreeSet<SubId>,
    /// Index into [`App::endpoints`] — the endpoint the throughput charts show.
    endpoint: usize,
    pub(crate) page: Page,
    pub(crate) range: Range,
    /// Why the live numbers are not moving, if they are not.
    pub(crate) feed_error: Option<String>,
    pub(crate) quit: bool,
}

impl App {
    pub(crate) fn new(snap: Snapshot) -> Self {
        Self {
            snap,
            allowance: libsubby::history::AllowanceHistory::default(),
            throughput: libsubby::history::Throughput::default(),
            cursor: 0,
            hidden: BTreeSet::new(),
            endpoint: 0,
            page: Page::Both,
            range: Range::Mixed,
            feed_error: None,
            quit: false,
        }
    }

    pub(crate) fn set_snapshot(&mut self, snap: Snapshot) {
        self.snap = snap;
        self.feed_error = None;
        // Accounts and pools come and go; neither index may point past the end.
        self.cursor = self.cursor.min(self.subs().len().saturating_sub(1));
        self.endpoint = self.endpoint.min(self.endpoints().len().saturating_sub(1));
    }

    /// The accounts, in the order the report lists them.
    pub(crate) fn subs(&self) -> &[SubView] {
        &self.snap.subs
    }

    /// `default` first, then each pool in file order. Empty while the proxy is
    /// down: an endpoint is a live listener, not a config stanza.
    pub(crate) fn endpoints(&self) -> Vec<Endpoint> {
        if self.snap.proxy.listening.is_none() {
            return Vec::new();
        }
        std::iter::once(Endpoint::Default)
            .chain(
                self.snap
                    .pools
                    .iter()
                    .map(|p| Endpoint::Pool(p.name.clone())),
            )
            .collect()
    }

    pub(crate) fn endpoint(&self) -> Option<Endpoint> {
        self.endpoints().get(self.endpoint).cloned()
    }

    pub(crate) fn is_shown(&self, id: SubId) -> bool {
        !self.hidden.contains(&id)
    }

    pub(crate) fn is_cursor(&self, id: SubId) -> bool {
        self.subs().get(self.cursor).is_some_and(|s| s.id == id)
    }

    pub(crate) fn shown(&self) -> Vec<&SubView> {
        self.subs().iter().filter(|s| self.is_shown(s.id)).collect()
    }

    /// Keyed on snapshot position, so a line keeps its colour across updates.
    pub(crate) fn hue_index(&self, id: SubId) -> usize {
        self.subs().iter().position(|s| s.id == id).unwrap_or(0)
    }

    /// Read off the **shown** accounts: a window nobody has cannot draw a line.
    pub(crate) fn charts(&self, page: Page) -> Vec<Chart> {
        let mut charts = Vec::new();
        if matches!(page, Page::Both | Page::Subs) {
            let mut windows: Vec<WindowKind> = Vec::new();
            for sub in self.shown() {
                if sub.session.is_some() && !windows.contains(&WindowKind::Session) {
                    windows.push(WindowKind::Session);
                }
                if sub.weekly.is_some() && !windows.contains(&WindowKind::Weekly) {
                    windows.push(WindowKind::Weekly);
                }
                for scoped in &sub.scoped {
                    let kind = WindowKind::Scoped(scoped.name.clone());
                    if !windows.contains(&kind) {
                        windows.push(kind);
                    }
                }
            }
            windows.sort_by_key(|w| match w {
                WindowKind::Session => (0, String::new()),
                WindowKind::Weekly => (1, String::new()),
                WindowKind::Scoped(name) => (2, name.clone()),
            });
            charts.extend(windows.into_iter().map(Chart::Allowance));
        }
        if matches!(page, Page::Both | Page::Proxy) && !self.endpoints().is_empty() {
            charts.push(Chart::Throughput(Metric::Tokens));
            charts.push(Chart::Throughput(Metric::Requests));
        }
        charts
    }

    /// The page drawn in `height` rows; one the user chose is never overridden.
    pub(crate) fn page_for(&self, height: u16, min_chart: u16) -> Page {
        if self.page != Page::Both {
            return self.page;
        }
        let charts = u16::try_from(self.charts(Page::Both).len()).unwrap_or(u16::MAX);
        if charts == 0 || height / charts.max(1) >= min_chart {
            Page::Both
        } else {
            Page::Subs
        }
    }

    pub(crate) fn window(&self, chart: &Chart, now: Timestamp) -> (Timestamp, Timestamp, Window) {
        let window = self.range.window(chart);
        (now - window.span, now, window)
    }

    pub(crate) fn quit(&mut self) {
        self.quit = true;
    }

    pub(crate) fn cursor_down(&mut self) {
        let len = self.subs().len();
        if len > 0 {
            self.cursor = (self.cursor + 1) % len;
        }
    }

    pub(crate) fn cursor_up(&mut self) {
        let len = self.subs().len();
        if len > 0 {
            self.cursor = (self.cursor + len - 1) % len;
        }
    }

    pub(crate) fn toggle(&mut self) {
        if let Some(sub) = self.subs().get(self.cursor) {
            let id = sub.id;
            if !self.hidden.remove(&id) {
                self.hidden.insert(id);
            }
        }
    }

    /// All shown means hide all, so "none, then pick two" beats four toggles.
    pub(crate) fn toggle_all(&mut self) {
        if self.hidden.is_empty() {
            self.hidden = self.subs().iter().map(|s| s.id).collect();
        } else {
            self.hidden.clear();
        }
    }

    /// One at a time, not overlaid: a shared axis flattens the smaller endpoint.
    pub(crate) fn next_endpoint(&mut self) {
        let len = self.endpoints().len();
        if len > 0 {
            self.endpoint = (self.endpoint + 1) % len;
        }
    }

    pub(crate) fn next_page(&mut self) {
        let at = Page::CYCLE
            .iter()
            .position(|p| *p == self.page)
            .unwrap_or(0);
        self.page = Page::CYCLE[(at + 1) % Page::CYCLE.len()];
    }

    pub(crate) fn set_range_index(&mut self, n: usize) {
        if let Some(range) = Range::ALL.get(n) {
            self.range = *range;
        }
    }

    pub(crate) fn wider(&mut self) {
        self.step_range(1);
    }

    pub(crate) fn narrower(&mut self) {
        self.step_range(-1);
    }

    /// Clamps rather than wrapping: `]` at the widest stays there.
    fn step_range(&mut self, delta: isize) {
        let at = Range::ALL
            .iter()
            .position(|r| *r == self.range)
            .unwrap_or(0);
        #[allow(clippy::cast_possible_wrap)]
        let next = (at as isize + delta).clamp(0, Range::ALL.len() as isize - 1);
        #[allow(clippy::cast_sign_loss)]
        {
            self.range = Range::ALL[next as usize];
        }
    }
}

#[cfg(test)]
mod tests {
    use libsubby::snapshot::{
        PoolView, ProxyView, RoutingView, ScopedWindow, SnapshotData, SubHealth, WindowView,
    };
    use libsubby::{CredentialSource, Provider, Severity, SubKey};

    use super::*;

    fn win(pct: f32) -> WindowView {
        WindowView {
            pct,
            resets_at: None,
            resets_in: None,
            severity: Severity::Ok,
            projection: None,
        }
    }

    fn sub(id: u32, provider: Provider, scoped: &[&str]) -> SubView {
        SubView {
            plan_tier: "unknown".into(),
            plan_weight: 1.0,
            id: SubId(id),
            key: SubKey::new(provider, format!("acct-{id}")),
            provider,
            label: format!("user{id}@example.com"),
            plan: Some("plus".to_owned()),
            source: CredentialSource::Keychain,
            enabled: true,
            health: SubHealth::Ok,
            // Codex Pro reports no session window; Claude reports one.
            session: (provider == Provider::Claude).then(|| win(10.0)),
            weekly: Some(win(20.0)),
            scoped: scoped
                .iter()
                .map(|name| ScopedWindow {
                    name: (*name).to_owned(),
                    window: win(3.0),
                })
                .collect(),
            routing: RoutingView {
                eligible: true,
                ..RoutingView::default()
            },
        }
    }

    fn app() -> App {
        App::new(Snapshot::from(SnapshotData {
            generation: 2,
            subs: vec![
                sub(1, Provider::Codex, &[]),
                sub(2, Provider::Claude, &["fable"]),
            ],
            pools: vec![PoolView {
                name: "moonshot".to_owned(),
                provider: None,
                members: vec![SubId(1)],
                eligible: vec![SubId(1)],
                max_session_pct: 100.0,
                max_weekly_pct: 100.0,
                openai_base_url: None,
                anthropic_base_url: None,
                proxied_in_flight: 0,
                proxied_tokens_1h: 0,
            }],
            proxy: ProxyView {
                running: true,
                listening: Some("127.0.0.1:8787".parse().unwrap()),
                ..ProxyView::default()
            },
            ..SnapshotData::default()
        }))
    }

    #[test]
    fn both_pages_are_every_window_that_exists_plus_the_two_rates() {
        let a = app();
        assert_eq!(
            a.charts(Page::Both),
            vec![
                Chart::Allowance(WindowKind::Session),
                Chart::Allowance(WindowKind::Weekly),
                Chart::Allowance(WindowKind::Scoped("fable".to_owned())),
                Chart::Throughput(Metric::Tokens),
                Chart::Throughput(Metric::Requests),
            ]
        );
        assert_eq!(a.charts(Page::Subs).len(), 3);
        assert_eq!(a.charts(Page::Proxy).len(), 2);
    }

    /// An endpoint is a live listener: with nothing bound, `p` has nowhere to go.
    #[test]
    fn a_stopped_proxy_has_no_endpoints_and_no_throughput_charts() {
        let mut a = app();
        a.set_snapshot(Snapshot::from(SnapshotData {
            generation: 3,
            subs: a.subs().to_vec(),
            ..SnapshotData::default()
        }));
        assert!(a.endpoints().is_empty());
        assert_eq!(a.endpoint(), None);
        assert_eq!(a.charts(Page::Proxy), Vec::new());
        a.next_endpoint();
        assert_eq!(a.endpoint(), None);
    }

    #[test]
    fn p_round_robins_the_endpoint_default_first() {
        let mut a = app();
        assert_eq!(a.endpoint(), Some(Endpoint::Default));
        a.next_endpoint();
        assert_eq!(a.endpoint(), Some(Endpoint::Pool("moonshot".to_owned())));
        a.next_endpoint();
        assert_eq!(a.endpoint(), Some(Endpoint::Default));
    }

    #[test]
    fn hiding_an_account_takes_its_windows_charts_with_it() {
        let mut a = app();
        assert!(
            a.charts(Page::Subs)
                .contains(&Chart::Allowance(WindowKind::Scoped("fable".to_owned())))
        );
        a.cursor_down();
        a.toggle();
        assert_eq!(
            a.charts(Page::Subs),
            vec![Chart::Allowance(WindowKind::Weekly)],
            "only the window the remaining account actually has"
        );
        a.cursor_up();
        assert!(a.is_cursor(SubId(1)));
        a.toggle();
        assert!(a.shown().is_empty());
        assert!(a.charts(Page::Subs).is_empty());
        // The rates are about endpoints, not accounts, and are unaffected.
        assert_eq!(a.charts(Page::Proxy).len(), 2);

        a.toggle_all();
        assert_eq!(a.shown().len(), 2);
    }

    #[test]
    fn mixed_gives_every_chart_its_own_natural_span() {
        let a = app();
        assert_eq!(a.range, Range::Mixed);
        let span = |c: &Chart| a.range.window(c).span;
        let session = span(&Chart::Allowance(WindowKind::Session));
        let weekly = span(&Chart::Allowance(WindowKind::Weekly));
        assert!(session < weekly);
        assert_eq!(
            span(&Chart::Allowance(WindowKind::Scoped("fable".into()))),
            weekly
        );
        assert!(span(&Chart::Throughput(Metric::Tokens)) < session);
    }

    /// The question Mixed cannot answer: all of them at the same moment.
    #[test]
    fn a_fixed_range_applies_to_every_chart() {
        let mut a = app();
        a.set_range_index(5);
        assert_eq!(a.range, Range::D7);
        let spans: Vec<_> = a
            .charts(Page::Both)
            .iter()
            .map(|chart| a.range.window(chart).span)
            .collect();
        assert!(spans.windows(2).all(|w| w[0] == w[1]), "{spans:?}");
        a.set_range_index(0);
        assert_eq!(a.range, Range::Mixed);
        a.set_range_index(99);
        assert_eq!(a.range, Range::Mixed);
    }

    #[test]
    fn the_range_ladder_clamps_rather_than_wrapping() {
        let mut a = app();
        a.narrower();
        assert_eq!(a.range, Range::Mixed);
        for _ in 0..10 {
            a.wider();
        }
        assert_eq!(a.range, Range::D7);
    }

    #[test]
    fn every_window_buckets_finer_than_any_pane_is_wide() {
        for range in Range::ALL {
            for chart in [
                Chart::Allowance(WindowKind::Session),
                Chart::Allowance(WindowKind::Weekly),
                Chart::Throughput(Metric::Tokens),
            ] {
                let w = range.window(&chart);
                let buckets = w.span.as_secs() / w.bucket.as_secs();
                assert!(buckets >= 40, "{w:?} -> {buckets}");
            }
        }
    }

    #[test]
    fn the_allowance_query_covers_the_widest_chart() {
        assert_eq!(Range::Mixed.allowance_span(), D7.span);
        assert_eq!(Range::Mixed.throughput_window(), H1);
        assert_eq!(Range::H6.allowance_span(), H6.span);
        assert_eq!(Range::H6.throughput_window(), H6);
    }

    #[test]
    fn both_degrades_to_subs_only_when_the_charts_would_be_unreadable() {
        let a = app();
        // Five charts, four rows each: room enough.
        assert_eq!(a.page_for(20, 4), Page::Both);
        // Five charts, two rows each: not enough for a slope.
        assert_eq!(a.page_for(10, 4), Page::Subs);
    }

    #[test]
    fn an_explicit_page_survives_a_short_terminal() {
        let mut a = app();
        a.next_page();
        assert_eq!(a.page, Page::Subs);
        assert_eq!(a.page_for(4, 4), Page::Subs);
        a.next_page();
        assert_eq!(a.page, Page::Proxy);
        assert_eq!(a.page_for(4, 4), Page::Proxy);
        a.next_page();
        assert_eq!(a.page, Page::Both);
    }

    #[test]
    fn both_indices_survive_a_shrinking_snapshot() {
        let mut a = app();
        a.cursor_down();
        a.next_endpoint();
        a.set_snapshot(Snapshot::from(SnapshotData {
            generation: 3,
            ..SnapshotData::default()
        }));
        assert!(a.subs().is_empty());
        assert!(a.endpoints().is_empty());
        assert_eq!(a.shown().len(), 0);
    }

    #[test]
    fn a_rate_chart_switches_to_per_hour_past_a_day() {
        let tokens = Chart::Throughput(Metric::Tokens);
        let requests = Chart::Throughput(Metric::Requests);
        for range in [Range::Mixed, Range::H1, Range::H6, Range::H12, Range::D1] {
            let w = range.window(&tokens);
            assert_eq!(w.rate, Rate::PerMinute, "{range:?}");
            assert_eq!(tokens.title(w), "TOK/MIN", "{range:?}");
        }
        let week = Range::D7.window(&tokens);
        assert_eq!(week.rate, Rate::PerHour);
        assert_eq!(tokens.title(week), "TOK/HR");
        assert_eq!(requests.title(week), "REQ/HR");
        // An allowance chart is a percentage, not a rate, and ignores it.
        assert_eq!(Chart::Allowance(WindowKind::Weekly).title(week), "WEEKLY");
    }
}
