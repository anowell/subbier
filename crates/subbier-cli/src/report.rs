//! The row layout both painters draw: `subbier status`'s ANSI text and the
//! TUI's pane. The two sections are not comparable — `SUBSCRIPTIONS` is
//! account-wide provider allowance, `PROXIES` only what we routed — so no
//! allowance bar appears under `PROXIES`, and no token count under the other.

use libsubby::history::Endpoint;
use libsubby::render;
use libsubby::snapshot::{PoolView, SubHealth, SubView, WindowView};
use libsubby::{Provider, Severity, Snapshot, SubId};

const METER_INDENT: &str = "    ";
const ENDPOINT_INDENT: &str = "  ";
const ENDPOINT_BODY_INDENT: &str = "    ";

/// [`Paint::Sev`] is the only run that ever carries a hue.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Paint {
    Plain,
    Dim,
    Bold,
    Sev(Severity),
    /// Something the user has to act on: an engine-level error.
    Alert,
}

#[derive(Debug)]
pub(crate) struct Span {
    pub text: String,
    pub paint: Paint,
}

#[derive(Debug)]
pub(crate) enum RowKind {
    /// One account's header line — the only row the cursor selects.
    Sub(SubId),
    /// An endpoint's name line, which starts a block the report scrolls by.
    Endpoint,
    Other,
}

#[derive(Debug)]
pub(crate) struct Row {
    pub kind: RowKind,
    pub spans: Vec<Span>,
}

impl Row {
    fn new(kind: RowKind) -> Self {
        Self {
            kind,
            spans: Vec::new(),
        }
    }

    fn other() -> Self {
        Self::new(RowKind::Other)
    }

    fn blank() -> Self {
        Self::other()
    }

    fn push(mut self, paint: Paint, text: impl Into<String>) -> Self {
        let text = text.into();
        // An empty run is escape codes around nothing, and a zero-width span.
        if !text.is_empty() {
            self.spans.push(Span { text, paint });
        }
        self
    }

    fn plain(self, text: impl Into<String>) -> Self {
        self.push(Paint::Plain, text)
    }

    fn dim(self, text: impl Into<String>) -> Self {
        self.push(Paint::Dim, text)
    }
}

/// The whole report, top to bottom. `width` is the meter bar's cell count: a
/// terminal report and a TUI pane are different widths.
pub(crate) fn build(snap: &Snapshot, width: usize) -> Vec<Row> {
    let mut rows = vec![Row::other().push(Paint::Bold, "SUBSCRIPTIONS")];

    if snap.subs.is_empty() {
        rows.push(
            Row::other()
                .dim("  No accounts yet — subbier adopts the ones codex and claude are already"),
        );
        rows.push(
            Row::other()
                .dim("  logged into. Run `codex login`, `claude login`, or `subbier login codex`."),
        );
    }
    for (i, sub) in snap.subs.iter().enumerate() {
        if i > 0 {
            rows.push(Row::blank());
        }
        rows.extend(sub_rows(sub, snap, width));
    }

    rows.push(Row::blank());
    rows.extend(proxy_rows(snap));

    if let Some(error) = &snap.last_error {
        rows.push(Row::blank());
        rows.push(
            Row::other()
                .push(Paint::Alert, "last error:")
                .plain(format!(" {error}")),
        );
    }
    rows
}

fn sub_rows(sub: &SubView, snap: &Snapshot, width: usize) -> Vec<Row> {
    let mut rows = vec![header(sub, snap)];

    // Pad every label to the same width so the bars form one column.
    let mut labels: Vec<&str> = vec!["Session", "Weekly"];
    labels.extend(sub.scoped.iter().map(|s| s.name.as_str()));
    let label_width = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0);

    let windows: Vec<(&str, &WindowView)> = std::iter::empty()
        .chain(sub.session.as_ref().map(|w| ("Session", w)))
        .chain(sub.weekly.as_ref().map(|w| ("Weekly", w)))
        .chain(sub.scoped.iter().map(|s| (s.name.as_str(), &s.window)))
        .collect();

    if windows.is_empty() {
        rows.push(
            Row::other()
                .plain(METER_INDENT)
                .dim("no allowance figures yet"),
        );
    }
    for (label, window) in windows {
        rows.push(meter(label, label_width, window, width));
    }
    rows
}

/// `Session  ████████░░░░░░░░░░░░   42%  · Resets in 4h 41m`.
///
/// Pads are applied before painting, so no escape codes count toward a column.
fn meter(label: &str, label_width: usize, window: &WindowView, width: usize) -> Row {
    let bar = render::bar(window.pct, width);
    let filled: String = bar.chars().take_while(|&c| c == '█').collect();
    let empty: String = bar.chars().skip(filled.chars().count()).collect();
    let percent = render::pct(window.pct);
    let sev = Paint::Sev(window.severity);

    let row = Row::other()
        .plain(format!("{METER_INDENT}{label:<label_width$}  "))
        .push(sev, filled)
        .dim(empty)
        .plain("  ")
        .push(sev, format!("{percent:>4}"));

    let mut trailer = Vec::new();
    match window.resets_in {
        // `duration` renders an elapsed reset as "now": "Resets in now" is not English.
        Some(d) if d.as_millis() <= 0 => trailer.push("Resets now".to_owned()),
        Some(d) => trailer.push(format!("Resets in {}", render::duration(d))),
        None => {}
    }
    if let Some(p) = &window.projection {
        trailer.push(format!(
            "on pace to exhaust in {}",
            render::duration(p.until_exhaustion)
        ));
    }
    if trailer.is_empty() {
        row
    } else {
        row.dim(format!("  · {}", trailer.join(" · ")))
    }
}

/// `* codex  anowell@example.com  (plus)  — disabled`
fn header(sub: &SubView, snap: &Snapshot) -> Row {
    // `*` marks the sub the router is currently on for its provider.
    let marker = if sub.routing.active { "* " } else { "  " };
    let mut row = Row::new(RowKind::Sub(sub.id)).plain(format!(
        "{marker}{:<7}{}",
        sub.provider.id(),
        sub.label
    ));
    if let Some(plan) = &sub.plan {
        row = row.dim(format!("  ({plan})"));
    }

    let mut notes: Vec<(Paint, String)> = Vec::new();
    if !sub.enabled {
        notes.push((Paint::Dim, "disabled".to_owned()));
    }
    if let Some((note, severity)) = health_note(sub, snap) {
        notes.push((
            match severity {
                Severity::Ok => Paint::Dim,
                other => Paint::Sev(other),
            },
            note,
        ));
    }
    if sub.enabled && !sub.routing.eligible && matches!(sub.health, SubHealth::Ok) {
        notes.push((Paint::Dim, "not eligible".to_owned()));
    }
    // Said in words as well as in colour, so the report survives a pipe.
    match worst_severity(sub) {
        Severity::Ok => {}
        severity => notes.push((Paint::Sev(severity), severity.to_string())),
    }

    if !notes.is_empty() {
        row = row.dim("  — ");
        for (i, (paint, note)) in notes.into_iter().enumerate() {
            if i > 0 {
                row = row.dim(", ");
            }
            row = row.push(paint, note);
        }
    }
    row
}

/// [`Severity::Ok`] here means "dim": worth stating, not worth colouring.
fn health_note(sub: &SubView, snap: &Snapshot) -> Option<(String, Severity)> {
    match &sub.health {
        SubHealth::Ok => None,
        SubHealth::Stale { since, error } => Some((
            format!(
                "stale for {} ({error})",
                render::duration(snap.captured_at.duration_since(*since))
            ),
            Severity::Ok,
        )),
        SubHealth::Unknown => Some(("never polled".to_owned(), Severity::Ok)),
        SubHealth::Exhausted { until } => Some((
            format!(
                "exhausted for another {}",
                render::duration(until.duration_since(snap.captured_at))
            ),
            Severity::Warn,
        )),
        SubHealth::NeedsLogin { error } => {
            Some((format!("needs login ({error})"), Severity::Critical))
        }
    }
}

fn worst_severity(sub: &SubView) -> Severity {
    std::iter::empty()
        .chain(sub.session.as_ref())
        .chain(sub.weekly.as_ref())
        .chain(sub.scoped.iter().map(|s| &s.window))
        .map(|w| w.severity)
        .max()
        .unwrap_or(Severity::Ok)
}

/// An endpoint is a live listener, so with the proxy down the block is a header.
fn proxy_rows(snap: &Snapshot) -> Vec<Row> {
    let proxy = &snap.proxy;
    let Some(addr) = proxy.listening else {
        let row = Row::other().push(Paint::Bold, "PROXIES:");
        return vec![if snap.settings.proxy_enabled {
            row.plain(" not running")
                .dim(format!(" · configured for {}", proxy.configured_bind))
        } else {
            row.dim(" disabled in config.kdl (proxy.enabled #false)")
        }];
    };

    let mut rows = vec![proxies_header(snap, addr)];
    rows.push(
        Row::new(RowKind::Endpoint).plain(format!("{ENDPOINT_INDENT}{}", Endpoint::Default.name())),
    );
    rows.push(Row::other().plain(format!(
        "{ENDPOINT_BODY_INDENT}{}",
        render::proxy_metrics(
            proxy.proxied_in_flight,
            proxy.proxied_tokens_1h,
            Some(proxy.proxied_requests_total),
        )
    )));
    for pool in &snap.pools {
        rows.extend(pool_rows(pool, snap));
    }
    rows
}

/// `PROXIES: lowest-usage on port 8787  · key required`
///
/// Only non-default settings earn a word; `--json` still carries them all.
fn proxies_header(snap: &Snapshot, addr: std::net::SocketAddr) -> Row {
    let s = &snap.settings;
    // On the normal loopback bind the port is the whole address a reader needs.
    let listening = if addr.ip().is_loopback() {
        format!("on port {}", addr.port())
    } else {
        format!("on {addr}")
    };
    let mut row = Row::other()
        .push(Paint::Bold, "PROXIES:")
        .plain(format!(" {} ", s.strategy))
        .dim(listening);

    let mut notes = Vec::new();
    if s.sticky != s.strategy.default_sticky() {
        notes.push(if s.sticky { "sticky" } else { "not sticky" }.to_owned());
    }
    if !s.auto_switch {
        notes.push("auto-switch off".to_owned());
    }
    let proxied: Vec<&Provider> = Provider::ALL.iter().filter(|p| s.proxies(**p)).collect();
    match proxied.as_slice() {
        [] => notes.push("no providers proxied".to_owned()),
        [only] => notes.push(format!("{} only", only.display_name())),
        _ => {}
    }
    if snap.proxy.requires_key {
        notes.push("key required".to_owned());
    }
    if !notes.is_empty() {
        row = row.dim(format!("  · {}", notes.join(" · ")));
    }
    row
}

/// Listed, not counted: "2 of 3 eligible" says nothing about which two.
fn pool_rows(pool: &PoolView, snap: &Snapshot) -> Vec<Row> {
    let mut name_row = Row::new(RowKind::Endpoint).plain(format!("{ENDPOINT_INDENT}{}", pool.name));

    let mut facts = Vec::new();
    if let Some(provider) = pool.provider {
        facts.push(format!("{} only", provider.display_name()));
    }
    // Two ceilings at the same number are one fact, not two.
    let (session, weekly) = (pool.max_session_pct, pool.max_weekly_pct);
    #[allow(clippy::float_cmp)]
    if session < 100.0 && session == weekly {
        facts.push(format!("under {} session & weekly", render::pct(session)));
    } else {
        for (name, pct) in [("session", session), ("weekly", weekly)] {
            if pct < 100.0 {
                facts.push(format!("under {} {name}", render::pct(pct)));
            }
        }
    }
    if !facts.is_empty() {
        name_row = name_row.dim(format!("  · {}", facts.join(" · ")));
    }

    let mut rows = vec![name_row];
    // The pool's own endpoint only; a pool keeps no lifetime total.
    rows.push(Row::other().plain(format!(
        "{ENDPOINT_BODY_INDENT}{}",
        render::proxy_metrics(pool.proxied_in_flight, pool.proxied_tokens_1h, None)
    )));
    rows.extend(
        pool.members
            .iter()
            .filter_map(|id| snap.subs.iter().find(|s| s.id == *id))
            .map(|sub| member(sub, pool)),
    );
    rows
}

/// `    codex  anowell@example.com`, dimmed with a reason when passed over.
fn member(sub: &SubView, pool: &PoolView) -> Row {
    let text = format!(
        "{ENDPOINT_BODY_INDENT}{:<7}{}",
        sub.provider.id(),
        sub.label
    );
    let row = Row::other();
    if pool.eligible.contains(&sub.id) {
        row.plain(text)
    } else {
        row.dim(format!(
            "{text} (inactive - {})",
            inactive_reason(sub, pool)
        ))
    }
}

/// Ordered the way the router filters — account usability first, then the pool's
/// ceilings, so a logged-out account is not blamed on a ceiling.
fn inactive_reason(sub: &SubView, pool: &PoolView) -> String {
    if !sub.enabled {
        return "disabled".to_owned();
    }
    match &sub.health {
        SubHealth::NeedsLogin { .. } => return "needs login".to_owned(),
        SubHealth::Exhausted { .. } => return "exhausted".to_owned(),
        SubHealth::Unknown => return "never polled".to_owned(),
        SubHealth::Ok | SubHealth::Stale { .. } => {}
    }
    if let Some(w) = sub.weekly
        && w.pct >= pool.max_weekly_pct
    {
        return format!("exceeds {} weekly usage", render::pct(pool.max_weekly_pct));
    }
    if let Some(w) = sub.session
        && w.pct >= pool.max_session_pct
    {
        return format!(
            "exceeds {} session usage",
            render::pct(pool.max_session_pct)
        );
    }
    "not eligible".to_owned()
}
