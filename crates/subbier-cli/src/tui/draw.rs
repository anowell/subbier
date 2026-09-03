//! Painting the TUI: [`crate::report`]'s rows on the left, charts on the right.
//! Two colour systems, split by place — the left column is severity, chart lines
//! are identity, from a ramp with no green, yellow or red so no plot grows a
//! traffic-light hue. They meet at an account's dot, so charts need no legend.

use libsubby::Severity;
use libsubby::history::MAX_HOLD;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Chart as ChartWidget, Dataset, GraphType, Paragraph};

use crate::report::{self, Paint, Row, RowKind};
use crate::tui::app::{App, Chart, Page};

/// The widest the report column may grow, as a fraction of the screen.
const LEFT_MAX_FRACTION: f32 = 0.62;

/// The box's two edges plus the three plot rows a slope needs.
const CHART_MIN_HEIGHT: u16 = 5;

/// Fixed and drawn by hand rather than by [`Axis::labels`]: ratatui sizes a
/// gutter to its own labels, starting `100` and `9.5K` plots in different columns.
const Y_GUTTER: u16 = 5;

/// Widest first: the first whose report fits the column budget wins.
const BAR_WIDTHS: [usize; 4] = [24, 20, 16, 12];

pub(crate) fn draw(frame: &mut Frame, app: &App) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());

    // Measured, so a one-account machine does not reserve a column for six.
    let rows = fitted_rows(app, body.width);
    let left = left_width(&rows, body.width);
    let [report_area, charts] =
        Layout::horizontal([Constraint::Length(left), Constraint::Min(0)]).areas(body);

    draw_live(frame, app, &rows, report_area);
    draw_charts(frame, app, charts);
    // The page the charts actually drew, decided by *their* height.
    draw_footer(
        frame,
        app,
        app.page_for(charts.height, CHART_MIN_HEIGHT),
        footer,
    );
}

/// The report, at the widest bar that leaves the charts their share.
fn fitted_rows(app: &App, total: u16) -> Vec<Row> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let budget = usize::from((f32::from(total) * LEFT_MAX_FRACTION) as u16);
    let mut rows = Vec::new();
    for width in BAR_WIDTHS {
        rows = report::build(&app.snap, width);
        if natural_width(&rows) <= budget {
            break;
        }
    }
    rows
}

fn natural_width(rows: &[Row]) -> usize {
    rows.iter()
        .map(|row| {
            row.spans
                .iter()
                .map(|s| s.text.chars().count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0)
        + GUTTER
}

/// Columns the selection gutter takes: cursor mark, chart mark, a space.
const GUTTER: usize = 3;

fn left_width(rows: &[Row], total: u16) -> u16 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let cap = (f32::from(total) * LEFT_MAX_FRACTION) as u16;
    u16::try_from(natural_width(rows) + 1)
        .unwrap_or(u16::MAX)
        .min(cap.max(1))
        .min(total)
}

fn draw_live(frame: &mut Frame, app: &App, rows: &[Row], area: Rect) {
    let lines: Vec<Line> = rows.iter().map(|row| live_line(app, row)).collect();
    frame.render_widget(
        Paragraph::new(lines).scroll((live_scroll(app, rows, area.height), 0)),
        area,
    );
}

fn live_line<'a>(app: &App, row: &'a Row) -> Line<'a> {
    let mut spans = match row.kind {
        RowKind::Sub(id) => {
            let cursor = if app.is_cursor(id) { "▸" } else { " " };
            // The dot is the chart's legend, beside the severity-coloured bar.
            let (mark, style) = if app.is_shown(id) {
                ("●", Style::new().fg(identity(app.hue_index(id))))
            } else {
                ("○", dim())
            };
            vec![
                Span::styled(cursor, Style::new().add_modifier(Modifier::BOLD)),
                Span::styled(mark, style),
                Span::raw(" "),
            ]
        }
        _ => vec![Span::raw(" ".repeat(GUTTER))],
    };
    spans.extend(
        row.spans
            .iter()
            .map(|s| Span::styled(&*s.text, style_of(s.paint))),
    );
    Line::from(spans)
}

/// The offset follows the cursor's *block*, so an account and the meters under
/// it come into view together.
fn live_scroll(app: &App, rows: &[Row], height: u16) -> u16 {
    let height = usize::from(height);
    if rows.len() <= height || height == 0 {
        return 0;
    }
    let Some(focus) = rows
        .iter()
        .position(|row| matches!(row.kind, RowKind::Sub(id) if app.is_cursor(id)))
    else {
        return 0;
    };
    let block_end = rows
        .iter()
        .skip(focus + 1)
        .position(|row| matches!(row.kind, RowKind::Sub(_) | RowKind::Endpoint))
        .map_or(rows.len(), |n| focus + 1 + n);

    let offset = block_end
        .saturating_sub(height)
        .min(rows.len() - height)
        // Never scroll the focused row off the top to fit its own block.
        .min(focus);
    u16::try_from(offset).unwrap_or(0)
}

/// [`Paint`] as a ratatui style — `style.rs`'s ANSI mapping, in ratatui.
fn style_of(paint: Paint) -> Style {
    match paint {
        Paint::Plain => Style::new(),
        Paint::Dim => dim(),
        Paint::Bold => Style::new().add_modifier(Modifier::BOLD),
        Paint::Alert => Style::new().fg(Color::Red),
        Paint::Sev(severity) => Style::new().fg(severity_hue(severity)),
    }
}

/// The traffic light in the terminal's own palette, which the user already
/// picked to work against their background.
fn severity_hue(severity: Severity) -> Color {
    match severity {
        Severity::Ok => Color::Green,
        Severity::Warn => Color::Yellow,
        Severity::Critical => Color::Red,
    }
}

/// The identity ramp — **no green, yellow or red, ever**, since an account that
/// landed on green would read as a healthy one.
fn identity(index: usize) -> Color {
    const RAMP: [Color; 6] = [
        Color::Cyan,
        Color::Magenta,
        Color::Blue,
        Color::LightCyan,
        Color::LightMagenta,
        Color::LightBlue,
    ];
    RAMP[index % RAMP.len()]
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn draw_charts(frame: &mut Frame, app: &App, area: Rect) {
    if area.width <= Y_GUTTER || area.height == 0 {
        return;
    }
    let page = app.page_for(area.height, CHART_MIN_HEIGHT);
    let charts = app.charts(page);
    if charts.is_empty() {
        frame.render_widget(Paragraph::new(Line::styled(empty_reason(app), dim())), area);
        return;
    }

    // The remainder goes to the earlier charts, so the last one ends flush.
    let count = u16::try_from(charts.len()).unwrap_or(1).max(1);
    let (height, mut extra) = (area.height / count, area.height % count);
    let mut top = area.y;
    for chart in &charts {
        let mut slot = height;
        if extra > 0 {
            slot += 1;
            extra -= 1;
        }
        if slot == 0 {
            break;
        }
        draw_chart(
            frame,
            app,
            chart,
            Rect {
                y: top,
                height: slot,
                ..area
            },
        );
        top += slot;
    }
}

fn empty_reason(app: &App) -> String {
    if app.subs().is_empty() {
        "  no accounts".to_owned()
    } else if app.page == Page::Proxy {
        "  no proxy endpoints — the proxy is not running".to_owned()
    } else if app.shown().is_empty() {
        "  no accounts selected — space to add one".to_owned()
    } else {
        "  no allowance figures yet".to_owned()
    }
}

fn draw_chart(frame: &mut Frame, app: &App, chart: &Chart, area: Rect) {
    let (since, until, window) = app.window(chart, jiff::Timestamp::now());
    let [gutter, plot] =
        Layout::horizontal([Constraint::Length(Y_GUTTER), Constraint::Min(1)]).areas(area);

    let mut title_spans = vec![
        Span::raw(" "),
        Span::styled(
            chart.title(window),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" · {}", window.label), dim()),
    ];
    // Not padding: `moonshot` alone reads as an account or a model.
    if let (Chart::Throughput(_), Some(endpoint)) = (chart, app.endpoint()) {
        title_spans.push(Span::styled(
            format!(" · {} proxy", endpoint.name()),
            Style::new().fg(Color::Cyan),
        ));
    }
    title_spans.push(Span::raw(" "));

    // The box *is* the chart, and fixes where zero is without a plot row for it.
    let block = Block::bordered()
        .border_style(dim())
        .title(Line::from(title_spans));
    let inner = block.inner(plot);
    if inner.width == 0 || inner.height == 0 {
        frame.render_widget(block, plot);
        return;
    }

    let points = usize::from(inner.width) * 2;
    let lines = series_for(app, chart, since, until, points, window);
    let ceiling = ceiling(chart, &lines);
    frame.render_widget(Paragraph::new(y_axis(chart, ceiling, area.height)), gutter);

    let datasets: Vec<Dataset> = lines
        .iter()
        .flat_map(|(color, runs)| {
            runs.iter().map(move |run| {
                Dataset::default()
                    .marker(symbols::Marker::Braille)
                    .graph_type(GraphType::Line)
                    .style(Style::new().fg(*color))
                    .data(run)
            })
        })
        .collect();

    // `points - 1`: the samples are indices `0..points`, so the last one has to
    // land on the last sub-column or every x drifts a fraction of a cell.
    #[allow(clippy::cast_precision_loss)]
    let x_max = points.saturating_sub(1).max(1) as f64;
    frame.render_widget(
        ChartWidget::new(datasets)
            .block(block)
            .x_axis(Axis::default().bounds([0.0, x_max]))
            .y_axis(Axis::default().bounds([0.0, ceiling])),
        plot,
    );

    if lines.iter().all(|(_, runs)| runs.is_empty()) {
        // A chart that draws nothing looks like a bug in the chart.
        frame.render_widget(
            Paragraph::new(Line::styled(" no history yet", dim())),
            Rect { height: 1, ..inner },
        );
    }
}

/// Per line: its colour, and its points split into runs of consecutive known
/// values, since a gap is a break rather than a straight segment across it.
type Plotted = Vec<(Color, Vec<Vec<(f64, f64)>>)>;

fn series_for(
    app: &App,
    chart: &Chart,
    since: jiff::Timestamp,
    until: jiff::Timestamp,
    points: usize,
    window: crate::tui::app::Window,
) -> Plotted {
    match chart {
        Chart::Allowance(window) => app
            .shown()
            .into_iter()
            .map(|sub| {
                let values = app
                    .allowance
                    .get(&sub.key, window)
                    // An allowance gap means nobody polled: a short one holds
                    // the last value, a long one breaks the line.
                    .map(|s| s.resample_held(since, until, points, MAX_HOLD))
                    .unwrap_or_default();
                (identity(app.hue_index(sub.id)), runs(values))
            })
            .collect(),
        Chart::Throughput(metric) => {
            let Some(endpoint) = app.endpoint() else {
                return Vec::new();
            };
            let values = app
                .throughput
                .rate(&endpoint, *metric, since, until, points, window.rate)
                .into_iter()
                // A throughput gap is a measured zero, never a break.
                .map(Some)
                .collect();
            // Plain foreground: an endpoint is not an account, nor a severity.
            (vec![(Color::Reset, runs(values))]).into_iter().collect()
        }
    }
}

/// A run of one point is dropped: a lone braille dot reads as noise.
fn runs(values: Vec<Option<f64>>) -> Vec<Vec<(f64, f64)>> {
    let mut out: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut run: Vec<(f64, f64)> = Vec::new();
    for (i, value) in values.into_iter().enumerate() {
        match value {
            #[allow(clippy::cast_precision_loss)]
            Some(v) => run.push((i as f64, v)),
            None => {
                if run.len() > 1 {
                    out.push(std::mem::take(&mut run));
                } else {
                    run.clear();
                }
            }
        }
    }
    if run.len() > 1 {
        out.push(run);
    }
    out
}

/// A percentage is drawn against `0..=100`, never its own peak (a flat 3%
/// autoscaled looks like a flat 95%); a rate takes the largest value on the plot.
fn ceiling(chart: &Chart, lines: &Plotted) -> f64 {
    match chart {
        Chart::Allowance(_) => 100.0,
        Chart::Throughput(_) => {
            let peak = lines
                .iter()
                .flat_map(|(_, runs)| runs.iter().flatten())
                .fold(0.0f64, |acc, (_, v)| acc.max(*v));
            if peak > 0.0 { peak } else { 1.0 }
        }
    }
}

/// A y-axis ceiling in **at most four characters** so it fits [`Y_GUTTER`],
/// dropping a decimal the moment it needs the room.
fn axis_label(v: f64) -> String {
    let v = if v.is_finite() { v.max(0.0) } else { 0.0 };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    if v < 1_000.0 {
        format!("{}", v.ceil() as u64)
    } else if v < 9_950.0 {
        format!("{:.1}K", v / 1e3)
    } else if v < 1e6 {
        format!("{}K", (v / 1e3).ceil())
    } else if v < 9.95e6 {
        format!("{:.1}M", v / 1e6)
    } else if v < 1e9 {
        format!("{}M", (v / 1e6).ceil())
    } else if v < 9.95e9 {
        format!("{:.1}B", v / 1e9)
    } else {
        // Saturates rather than widening the gutter.
        format!("{}B", (v / 1e9).ceil().min(999.0))
    }
}

/// A ceiling and a floor label, each on the row inside the box the value lands
/// on. The box's left edge is the axis, so the gutter draws no rule and no `┤`.
fn y_axis<'a>(chart: &Chart, ceiling: f64, height: u16) -> Vec<Line<'a>> {
    let top = match chart {
        Chart::Allowance(_) => "100".to_owned(),
        Chart::Throughput(_) => axis_label(ceiling),
    };
    // Row 0 and the last row are the box's own edges; the plot is between them.
    (0..height)
        .map(|row| {
            let label = if row == 1 {
                top.as_str()
            } else if row + 2 == height {
                "0"
            } else {
                ""
            };
            Line::styled(format!("{label:>4} "), dim())
        })
        .collect()
}

fn draw_footer(frame: &mut Frame, app: &App, page: Page, area: Rect) {
    if let Some(error) = &app.feed_error {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" not updating: ", Style::new().fg(Color::Red)),
                Span::styled(error.as_str(), dim()),
            ])),
            area,
        );
        return;
    }
    let keys: [(&str, String); 6] = [
        ("j/k", "sub".to_owned()),
        ("space", "chart".to_owned()),
        ("a", "all".to_owned()),
        (
            "p",
            app.endpoint()
                .map_or("pool".to_owned(), |e| e.name().to_owned()),
        ),
        ("tab", format!("page ({})", page.label())),
        ("0-5", format!("range ({})", app.range.label())),
    ];
    let mut spans = vec![Span::raw(" ")];
    for (i, (key, what)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", dim()));
        }
        spans.push(Span::styled(
            *key,
            Style::new().add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(format!(" {what}"), dim()));
    }
    spans.push(Span::styled(" · ", dim()));
    spans.push(Span::styled("q", Style::new().add_modifier(Modifier::BOLD)));
    spans.push(Span::styled(" quit", dim()));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use jiff::SignedDuration;
    use libsubby::history::{AllowanceHistory, Throughput};
    use libsubby::snapshot::{
        PoolView, ProxyView, RoutingView, ScopedWindow, SnapshotData, SubHealth, SubView,
        WindowView,
    };
    use libsubby::store::db::{AllowanceRow, ProxiedBucket};
    use libsubby::{CredentialSource, Provider, Snapshot, SubId, SubKey, WindowKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn window(pct: f32, severity: Severity, resets_in: i64) -> WindowView {
        WindowView {
            pct,
            resets_at: None,
            resets_in: Some(SignedDuration::from_hours(resets_in)),
            severity,
            projection: None,
        }
    }

    fn sub(id: u32, provider: Provider, plan: &str, weekly: f32, sev: Severity) -> SubView {
        SubView {
            plan_tier: "unknown".into(),
            plan_weight: 1.0,
            id: SubId(id),
            key: SubKey::new(provider, format!("acct-{id}")),
            provider,
            label: "anthony@howie.ai".to_owned(),
            plan: Some(plan.to_owned()),
            source: CredentialSource::Keychain,
            enabled: true,
            health: SubHealth::Ok,
            session: Some(window(6.0, Severity::Ok, 4)),
            weekly: Some(window(weekly, sev, 132)),
            scoped: if provider == Provider::Claude {
                vec![ScopedWindow {
                    name: "fable".to_owned(),
                    window: window(1.0, Severity::Ok, 132),
                }]
            } else {
                Vec::new()
            },
            routing: RoutingView {
                eligible: true,
                active: id == 1,
                ..RoutingView::default()
            },
        }
    }

    fn fixture() -> App {
        let snap = Snapshot::from(SnapshotData {
            generation: 2,
            subs: vec![
                sub(1, Provider::Codex, "pro", 11.0, Severity::Ok),
                sub(2, Provider::Claude, "max", 91.0, Severity::Critical),
            ],
            pools: vec![PoolView {
                name: "moonshot".to_owned(),
                provider: None,
                members: vec![SubId(1)],
                eligible: vec![SubId(1)],
                max_session_pct: 50.0,
                max_weekly_pct: 50.0,
                openai_base_url: None,
                anthropic_base_url: None,
                proxied_in_flight: 0,
                proxied_tokens_1h: 0,
            }],
            proxy: ProxyView {
                running: true,
                listening: Some("127.0.0.1:8787".parse().unwrap()),
                proxied_in_flight: 6,
                proxied_requests_total: 7728,
                proxied_tokens_1h: 5_500_000,
                ..ProxyView::default()
            },
            ..SnapshotData::default()
        });
        let mut app = App::new(snap);

        let now = jiff::Timestamp::now();
        let mut rows = Vec::new();
        for step in 0..400 {
            let ts = now - SignedDuration::from_mins(7 * 24 * 60 - step * 25);
            #[allow(clippy::cast_precision_loss)]
            let ramp = step as f32 / 400.0;
            for (key, scale) in [
                (SubKey::new(Provider::Codex, "acct-1"), 11.0),
                (SubKey::new(Provider::Claude, "acct-2"), 91.0),
            ] {
                rows.push(AllowanceRow {
                    ts,
                    sub: key.clone(),
                    window: WindowKind::Weekly,
                    pct: scale * ramp,
                });
                rows.push(AllowanceRow {
                    ts,
                    sub: key.clone(),
                    window: WindowKind::Session,
                    // A sawtooth, so the session chart has resets in it.
                    pct: scale * ((step % 40) as f32 / 40.0),
                });
                rows.push(AllowanceRow {
                    ts,
                    sub: key,
                    window: WindowKind::Scoped("fable".to_owned()),
                    pct: scale * ramp * 0.2,
                });
            }
        }
        app.allowance = AllowanceHistory::from_rows(rows);

        let bucket = app.range.throughput_window().bucket;
        let buckets = (0..60)
            .map(|i| ProxiedBucket {
                ts: now - SignedDuration::from_mins(60 - i),
                route: "/v1/messages".to_owned(),
                requests: u32::try_from(i % 13).unwrap_or(0),
                tokens: u64::try_from(i % 13).unwrap_or(0) * 30_000,
            })
            .collect();
        app.throughput = Throughput::from_buckets(buckets, bucket);
        app
    }

    fn render(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn braille_rows(app: &App, width: u16, height: u16) -> Vec<u16> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .filter(|y| {
                (0..buffer.area.width).any(|x| {
                    buffer[(x, *y)]
                        .symbol()
                        .chars()
                        .all(|c| ('\u{2801}'..='\u{28ff}').contains(&c))
                })
            })
            .collect()
    }

    /// Zero lands on the last row inside the box, above its bottom edge.
    #[test]
    fn a_zero_value_lands_on_the_row_the_axis_calls_zero() {
        let mut app = fixture();
        app.next_page();
        app.next_page(); // proxy only: two rate charts

        let now = jiff::Timestamp::now();
        app.throughput = Throughput::from_buckets(
            (0..60)
                .map(|i| ProxiedBucket {
                    ts: now - SignedDuration::from_mins(60 - i),
                    route: "/v1/messages".to_owned(),
                    requests: 0,
                    tokens: 0,
                })
                .collect(),
            app.range.throughput_window().bucket,
        );

        let rows = braille_rows(&app, 140, 20);
        let screen = render(&app, 140, 20);
        let painted: Vec<&str> = screen.lines().collect();
        assert_eq!(rows.len(), 2, "one flat line per rate chart:\n{screen}");
        for y in rows {
            assert!(
                painted[usize::from(y)].contains("   0 │"),
                "row {y} is not the row the axis calls zero:\n{screen}"
            );
        }
    }

    #[test]
    fn each_chart_is_a_box_around_an_otherwise_empty_plot() {
        let mut app = fixture();
        app.allowance = AllowanceHistory::default();
        app.throughput = Throughput::default();

        app.next_page(); // subs only: three allowance charts
        let screen = render(&app, 140, 26);
        for corner in ['┌', '┐', '└', '┘'] {
            assert_eq!(
                screen.chars().filter(|c| *c == corner).count(),
                3,
                "one {corner} per chart:\n{screen}"
            );
        }
        assert!(screen.contains("───"), "no drawn edge:\n{screen}");
        assert!(
            braille_rows(&app, 140, 26).is_empty(),
            "nothing is plotted, so nothing should be marked:\n{screen}"
        );
        // An empty plot says why, rather than looking like a broken chart.
        assert!(screen.contains("no history yet"), "{screen}");
    }

    #[test]
    fn the_box_is_dim_and_the_lines_are_not() {
        let app = fixture();
        let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        let (mut data_cells, mut box_cells) = (0, 0);
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                if matches!(cell.symbol(), "┌" | "┐" | "└" | "┘" | "─" | "│") {
                    box_cells += 1;
                    assert!(cell.modifier.contains(Modifier::DIM), "({x},{y})");
                } else if cell
                    .symbol()
                    .chars()
                    .all(|c| ('\u{2800}'..='\u{28ff}').contains(&c))
                    && (0..6).any(|i| cell.fg == identity(i))
                {
                    data_cells += 1;
                    assert!(!cell.modifier.contains(Modifier::DIM), "({x},{y})");
                }
            }
        }
        assert!(data_cells > 0, "no plotted data at all");
        assert!(box_cells > 0, "no box drawn at all");
    }

    #[test]
    fn the_y_gutter_has_no_tick_pointing_where_nothing_is_drawn() {
        let screen = render(&fixture(), 140, 36);
        assert!(!screen.contains('┤'), "{screen}");
        assert!(screen.contains(" 100 │"), "{screen}");
        assert!(screen.contains("   0 │"), "{screen}");
    }

    #[test]
    fn an_axis_label_always_fits_its_gutter() {
        for v in [
            0.0,
            1.0,
            999.0,
            1_000.0,
            4_500.0,
            9_949.0,
            12_000.0,
            360_000.0,
            999_000.0,
            1_200_000.0,
            9_900_000.0,
            12_000_000.0,
            305_200_000.0,
            3_200_000_000.0,
            42_000_000_000.0,
        ] {
            let label = axis_label(v);
            assert!(label.chars().count() <= 4, "{v} -> {label:?}");
        }
        // A non-finite ceiling is "we measured nothing", not an axis of NaN.
        assert_eq!(axis_label(f64::NAN), "0");
        assert_eq!(axis_label(-5.0), "0");
    }

    /// The report is measured, and the bar shrinks before the charts do.
    #[test]
    fn the_report_column_never_takes_more_than_its_share() {
        let app = fixture();
        for width in [80u16, 100, 140, 200] {
            let rows = fitted_rows(&app, width);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let cap = (f32::from(width) * LEFT_MAX_FRACTION) as u16;
            assert!(left_width(&rows, width) <= cap.max(1), "{width}");
        }
        // A wide terminal keeps the widest bar; a narrow one gives it up.
        assert!(natural_width(&fitted_rows(&app, 200)) > natural_width(&fitted_rows(&app, 90)));
    }

    /// The one rule that keeps two colour systems from colliding.
    #[test]
    fn the_identity_ramp_never_borrows_a_traffic_light_hue() {
        let severity = [Color::Green, Color::Yellow, Color::Red];
        for i in 0..24 {
            assert!(!severity.contains(&identity(i)), "slot {i}");
        }
        // ... and distinct across the accounts anyone has.
        let first: Vec<Color> = (0..6).map(identity).collect();
        for (i, c) in first.iter().enumerate() {
            assert_eq!(*c, identity(i));
            assert_eq!(first.iter().filter(|o| *o == c).count(), 1);
        }
    }

    #[test]
    fn charts_all_fit_a_tall_terminal_and_page_on_a_short_one() {
        let tall = render(&fixture(), 140, 36);
        for title in ["SESSION", "WEEKLY", "FABLE", "TOK/MIN", "REQ/MIN"] {
            assert!(tall.contains(title), "{title} missing:\n{tall}");
        }

        let short = render(&fixture(), 100, 18);
        assert!(short.contains("WEEKLY"), "{short}");
        assert!(!short.contains("TOK/MIN"), "{short}");
        assert!(short.contains("page (subs)"), "{short}");
    }

    #[test]
    fn hiding_an_account_removes_the_charts_only_it_had() {
        let mut app = fixture();
        assert!(render(&app, 140, 36).contains("FABLE"));
        app.cursor_down();
        app.toggle(); // hide the Claude account
        let screen = render(&app, 140, 36);
        assert!(!screen.contains("FABLE"), "{screen}");
        assert!(screen.contains("WEEKLY"), "{screen}");
        // The rate charts are about endpoints, not accounts, and stand.
        app.cursor_up();
        app.toggle();
        let none = render(&app, 140, 36);
        assert!(!none.contains("WEEKLY"), "{none}");
        assert!(none.contains("TOK/MIN"), "{none}");

        // With the proxy down too, the column says which key brings it back.
        app.set_snapshot(Snapshot::from(SnapshotData {
            generation: 3,
            subs: app.subs().to_vec(),
            ..SnapshotData::default()
        }));
        let empty = render(&app, 140, 36);
        assert!(empty.contains("no accounts selected"), "{empty}");
    }

    /// `p` names the endpoint on the chart that changed, not in a legend.
    #[test]
    fn p_moves_the_throughput_charts_to_the_next_endpoint() {
        let mut app = fixture();
        assert!(render(&app, 140, 36).contains("· default proxy"), "default");
        app.next_endpoint();
        let screen = render(&app, 140, 36);
        assert!(screen.contains("· moonshot proxy"), "{screen}");
        assert!(screen.contains("p moonshot"), "{screen}");
    }

    /// The dot in the report is the chart's legend, so it goes with the line.
    #[test]
    fn space_removes_an_account_from_the_charts() {
        let mut app = fixture();
        assert!(render(&app, 140, 36).contains("▸●"), "shown");
        app.toggle();
        let screen = render(&app, 140, 36);
        assert!(screen.contains("▸○"), "{screen}");
        app.toggle_all();
        assert!(render(&app, 140, 36).contains("▸●"), "restored");
    }

    #[test]
    fn chart_lines_are_identity_hues_and_report_bars_are_severity_hues() {
        let app = fixture();
        let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        let mut line_hues = std::collections::BTreeSet::new();
        let mut bar_hues = std::collections::BTreeSet::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                let symbol = cell.symbol();
                // Braille is a plotted line; a full block is a meter bar.
                if symbol
                    .chars()
                    .all(|c| ('\u{2800}'..='\u{28ff}').contains(&c))
                {
                    line_hues.insert(format!("{:?}", cell.fg));
                } else if symbol == "█" {
                    bar_hues.insert(format!("{:?}", cell.fg));
                }
            }
        }

        assert!(
            line_hues.contains(&format!("{:?}", identity(0))),
            "{line_hues:?}"
        );
        assert!(
            line_hues.contains(&format!("{:?}", identity(1))),
            "{line_hues:?}"
        );
        for severity in [Severity::Ok, Severity::Warn, Severity::Critical] {
            assert!(
                !line_hues.contains(&format!("{:?}", severity_hue(severity))),
                "a traffic-light hue got onto a plot: {line_hues:?}"
            );
        }
        assert!(!bar_hues.is_empty());
        for hue in &bar_hues {
            assert!(
                [Severity::Ok, Severity::Warn, Severity::Critical]
                    .iter()
                    .any(|s| *hue == format!("{:?}", severity_hue(*s))),
                "an identity hue got onto a bar: {hue}"
            );
        }
    }

    /// `j` past the fold must not move a cursor nobody can see.
    #[test]
    fn the_report_column_scrolls_to_keep_the_cursor_in_view() {
        let mut app = fixture();
        let rows = fitted_rows(&app, 140);
        assert_eq!(live_scroll(&app, &rows, 100), 0, "no scroll when it fits");
        app.cursor_down();
        let scrolled = live_scroll(&app, &rows, 4);
        assert!(scrolled > 0, "cursor below the fold should scroll");
    }
}
