//! The menu: `Snapshot -> set_text/set_checked`, and clicks -> `Command`.
//!
//! Main-thread-only: muda and tray-icon handles are `!Send`, hence the `thread_local!`.
//! Updates are in place except the per-sub block; a rebuild flickers and drops the highlight.

use std::cell::RefCell;
use std::time::Duration;

use libsubby::render;
use libsubby::snapshot::{SnapshotData, SubView};
use libsubby::{Command, Handle, Provider, Severity, Snapshot, SubId};
use muda::{IsMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem};
use objc2::rc::Retained;
use objc2_foundation::{MainThreadMarker, NSString};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::menu::controls::{COPY_ENV, Controls};
use crate::menu::rowview::RowView;
use crate::menu::{self, Owner, PREFIX_ROWS, TabBar, TabId, hatch, subrow};
use crate::{env, icon, login_item, pasteboard};

/// How long "Copy env snippet" acknowledges the copy before reverting.
const COPIED_FEEDBACK: Duration = Duration::from_millis(1400);

/// Between `Shutdown` and `exit(0)`, so a setting toggled just before quitting still lands.
const QUIT_GRACE: Duration = Duration::from_millis(250);

/// What `Edit config.kdl…` writes when there is no config file yet: every key has a
/// default, so a blank file would work — and teach nothing.
const NEW_CONFIG: &str = "\
// subbier — everything here is optional; the defaults are a working install.
// See the README for the whole set of keys.
//
// A pool is a slice of your accounts on its own URL, so one shell cannot spend
// what another one is saving:
//
// pool \"moonshot\" {
//     sub \"spare@example.com\"        // or `sub codex \"spare@example.com\"`
//     max-sub-weekly-utilization 0.5   // stop at half of each account's week
// }
";

thread_local! {
    /// `!Send` by construction, which is why it is here rather than in a `static`.
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

/// Build the status item and its menu. **Main thread, and the run loop must already
/// be running**: tray-icon requires both. Panics if the status item cannot be created.
pub fn install(handle: Handle, rt: tokio::runtime::Handle) {
    let mtm = MainThreadMarker::new().expect("the menu is built off the main thread");
    let ui = Ui::new(handle, rt, mtm);
    let snapshot = ui.handle.snapshot();
    UI.with_borrow_mut(|slot| *slot = Some(ui));
    apply(&snapshot);
    menu::install_tracking_ticker(mtm);
}

/// Push a snapshot into the menu. **Main thread, menu closed.**
pub fn apply(snapshot: &Snapshot) {
    UI.with_borrow_mut(|slot| {
        if let Some(ui) = slot.as_mut() {
            ui.apply(snapshot);
        }
    });
}

/// Push the newest snapshot into a menu that is **open**: called from the
/// `NSEventTrackingRunLoopMode` timer, the only context that runs while a menu tracks.
pub(crate) fn tick_while_tracking() {
    with_ui(|ui| {
        let snapshot = ui.handle.snapshot();
        if ui.applied_generation != Some(snapshot.generation) {
            ui.apply(&snapshot);
        }
    });
}

/// Switch tab, from inside the menu's tracking loop. **Main thread.** The control block
/// is repainted too: a tab chooses *whose proxy* the `Proxy ▸` row describes.
pub(crate) fn select_tab(tab: TabId) {
    with_ui(|ui| {
        ui.tabs.select(&ui.menu, tab);
        let snapshot = ui.handle.snapshot();
        ui.update_controls(&snapshot);
    });
}

/// Reach the `Ui` from inside the menu's tracking loop. A panic here would unwind through
/// Objective-C and take the app with it, so a re-entrant borrow skips the frame instead.
fn with_ui(f: impl FnOnce(&mut Ui)) {
    UI.with(|cell| match cell.try_borrow_mut() {
        Ok(mut slot) => {
            if let Some(ui) = slot.as_mut() {
                f(ui);
            }
        }
        Err(_) => tracing::debug!("skipped a tracking-loop update: the Ui was already borrowed"),
    });
}

/// Turn a menu click into a [`Command`]. **Main thread.**
pub fn on_menu_event(id: &MenuId) {
    UI.with_borrow_mut(|slot| {
        if let Some(ui) = slot.as_mut() {
            ui.on_menu_event(id.as_ref());
        }
    });
}

pub struct Ui {
    handle: Handle,
    rt: tokio::runtime::Handle,
    mtm: MainThreadMarker,
    tray: TrayIcon,
    menu: Menu,

    /// The **view** is the handle, not the menu index: muda allocates a fresh
    /// `NSMenuItem` on every `insert`, but the view we hung on it outlives the item.
    sub_rows: Vec<(SubId, Retained<RowView>)>,
    status: Option<MenuItem>,
    prefix_len: usize,
    shape: Shape,
    /// So the tracking-mode timer can skip a snapshot it has already applied.
    applied_generation: Option<u64>,

    tabs: TabBar,
    /// Who owns each row of the fixed block below the prefix, in menu order.
    tail_owners: Vec<Owner>,
    controls: Controls,

    /// Tracked so a redundant `set_icon`, which decodes the PNG again, is skipped.
    icon_shown: bool,
}

/// What the rebuildable prefix was built for; anything else is a `set_text` away.
#[derive(Debug, Default, PartialEq, Eq)]
struct Shape {
    subs: Vec<SubId>,
    /// Membership too: a pool whose members changed shows a different set of rows.
    pools: Vec<(String, Vec<SubId>)>,
    has_status: bool,
}

impl Shape {
    fn of(snapshot: &SnapshotData, has_status: bool) -> Self {
        Self {
            subs: snapshot.subs.iter().map(|sub| sub.id).collect(),
            pools: snapshot
                .pools
                .iter()
                .map(|pool| (pool.name.clone(), pool.members.clone()))
                .collect(),
            has_status,
        }
    }
}

impl Ui {
    fn new(handle: Handle, rt: tokio::runtime::Handle, mtm: MainThreadMarker) -> Self {
        let menu = Menu::new();
        let controls = Controls::new();
        let quit = MenuItem::with_id("quit", "Quit subbier", true, None);

        // The tab strip's row: its title is never seen and its action never performed,
        // because the view covers the whole row.
        let tab_row = MenuItem::with_id("tabs", "", true, None);
        let strip_separator = PredefinedMenuItem::separator();
        let tail_separator = PredefinedMenuItem::separator();

        // Row and owner together, or the map drifts and hides the wrong rows.
        let tail_owners = {
            let head: [(&dyn IsMenuItem, Owner); PREFIX_ROWS] =
                [(&tab_row, Owner::Common), (&strip_separator, Owner::Common)];
            let trailing: [(&dyn IsMenuItem, Owner); 2] =
                [(&tail_separator, Owner::Common), (&quit, Owner::Common)];
            for (item, _) in head {
                let _ = menu.append(item);
            }
            let mut owners = Vec::new();
            for row in controls.rows() {
                let _ = menu.append(row.item);
                owners.push(row.owner);
            }
            for (item, owner) in trailing {
                let _ = menu.append(item);
                owners.push(owner);
            }
            owners
        };

        // `with_icon_as_template(true)` is not optional: the asset is black + alpha
        // and invisible on a dark menu bar without it.
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_icon(icon::menu_bar_icon())
            .with_icon_as_template(true)
            .with_tooltip("subbier")
            .build()
            .expect("the status item could not be created");

        Self {
            handle,
            rt,
            mtm,
            tray,
            menu,
            sub_rows: Vec::new(),
            status: None,
            prefix_len: 0,
            shape: Shape::default(),
            applied_generation: None,
            tabs: TabBar::new(mtm),
            tail_owners,
            controls,
            icon_shown: true,
        }
    }

    fn apply(&mut self, snapshot: &SnapshotData) {
        let status = status_line(snapshot);
        let shape = Shape::of(snapshot, status.is_some());
        if shape != self.shape {
            self.rebuild_prefix(snapshot, status.as_deref());
            self.shape = shape;
        }

        if let (Some(item), Some(text)) = (self.status.as_ref(), status.as_ref()) {
            item.set_text(text);
        }
        self.update_tabs(snapshot);
        self.update_rows(snapshot);
        self.update_controls(snapshot);
        self.update_status_item(snapshot);
        self.applied_generation = Some(snapshot.generation);
    }

    /// **Keeps whatever tab is selected**: re-resolving it here threw the user back
    /// to `All subs` every few seconds with traffic in flight.
    fn update_tabs(&mut self, snapshot: &SnapshotData) {
        let tabs = menu::available_tabs(snapshot);
        let wanted = self.tabs.selected();
        if self.tabs.set_tabs(tabs, wanted) {
            self.tabs.refresh(&self.menu);
        }
    }

    /// Replace the per-sub block, when the subs or the status line change.
    ///
    /// muda allocates a **fresh `NSMenuItem` on every `insert`**, so everything the hatch
    /// applied is gone: each row is painted in the loop that inserts it, and
    /// [`TabBar::apply`] runs last, against the same map.
    fn rebuild_prefix(&mut self, snapshot: &SnapshotData, status: Option<&str>) {
        for _ in 0..self.prefix_len {
            self.menu.remove_at(PREFIX_ROWS);
        }
        self.sub_rows.clear();
        self.status = None;
        // Retained once: inserting and removing rows replaces the items, not the `NSMenu`.
        let ns_menu = hatch::ns_menu(&self.menu);

        // Seeded with the fixed head, so `owners.len()` is the index to insert at.
        let mut owners: Vec<Owner> = vec![Owner::Common; PREFIX_ROWS];

        if let Some(text) = status {
            let item = info(text);
            insert(&self.menu, &mut owners, &item, Owner::Common);
            self.status = Some(item);
        }

        if snapshot.subs.is_empty() {
            let text = if snapshot.generation == 0 {
                "Looking for accounts…"
            } else {
                "No accounts — add one in Settings ▸"
            };
            insert(&self.menu, &mut owners, &info(text), Owner::Common);
            insert(
                &self.menu,
                &mut owners,
                &PredefinedMenuItem::separator(),
                Owner::Common,
            );
        }

        // Every tab gets its own complete set of rows, hidden until selected: one row
        // cannot be in two places.
        for tab in self.rendered_tabs(snapshot) {
            let owner = Owner::Tab(tab);
            // `SESSION  WEEKLY` over nothing is worse than no header at all.
            let any = snapshot
                .subs
                .iter()
                .any(|sub| menu::tab_shows(snapshot, tab, sub.id));

            if any {
                insert_row(
                    &self.menu,
                    &ns_menu,
                    &mut owners,
                    self.mtm,
                    subrow::Row::Columns,
                    owner,
                    None,
                );
            }

            for provider in Provider::ALL {
                let subs: Vec<&SubView> = snapshot
                    .subs
                    .iter()
                    .filter(|sub| sub.provider == provider)
                    .filter(|sub| menu::tab_shows(snapshot, tab, sub.id))
                    .collect();
                if subs.is_empty() {
                    continue;
                }

                // Nothing on the section rule changes, so it is never revisited.
                insert_row(
                    &self.menu,
                    &ns_menu,
                    &mut owners,
                    self.mtm,
                    subrow::section_of(provider.display_name()),
                    owner,
                    None,
                );

                for sub in subs {
                    let row = insert_row(
                        &self.menu,
                        &ns_menu,
                        &mut owners,
                        self.mtm,
                        subrow::row_of(sub),
                        owner,
                        Some(&subrow::tooltip(sub)),
                    );
                    self.sub_rows.push((sub.id, row));
                }
            }

            // A pool can name accounts not logged in yet; saying so beats a tab that
            // looks broken.
            if !any && !snapshot.subs.is_empty() {
                insert(
                    &self.menu,
                    &mut owners,
                    &info("No accounts in this pool"),
                    owner,
                );
            }

            // Tagged with the tab, or a stray line survives under the hidden rows.
            insert(
                &self.menu,
                &mut owners,
                &PredefinedMenuItem::separator(),
                owner,
            );
        }

        self.prefix_len = owners.len() - PREFIX_ROWS;
        owners.extend_from_slice(&self.tail_owners);

        // Before the map goes back to AppKit: a selection that no longer exists would
        // hide the whole menu.
        let selected = self.tabs.selected();
        self.tabs.set_tabs(menu::available_tabs(snapshot), selected);
        self.tabs.apply(&self.menu, owners);
    }

    /// With no pools the strip is hidden, but rows still need an owner the selection can
    /// match — [`TabBar`] falls back to [`TabId::All`].
    fn rendered_tabs(&self, snapshot: &SnapshotData) -> Vec<TabId> {
        let tabs = menu::available_tabs(snapshot);
        if tabs.is_empty() {
            vec![TabId::All]
        } else {
            tabs.into_iter().map(|tab| tab.id).collect()
        }
    }

    /// Matched by [`SubId`], never by position: rows are grouped by provider, the
    /// snapshot is not.
    fn update_rows(&self, snapshot: &SnapshotData) {
        for (id, view) in &self.sub_rows {
            if let Some(sub) = snapshot.subs.iter().find(|sub| sub.id == *id) {
                view.set_row(subrow::row_of(sub));
                view.setToolTip(Some(&NSString::from_str(&subrow::tooltip(sub))));
            }
        }
    }

    /// The proxy toggle's metrics live in an **attributed** title, which `set_text` cannot
    /// touch and a prefix rebuild throws away — so painting happens on every pass.
    fn update_controls(&self, snapshot: &SnapshotData) {
        self.controls.sync(snapshot);
        // The engine records the intent; the LaunchAgent that makes it true is ours.
        login_item::reconcile(snapshot.settings.launch_at_login);

        let ns_menu = hatch::ns_menu(&self.menu);
        self.controls.paint(
            &ns_menu,
            PREFIX_ROWS + self.prefix_len,
            snapshot,
            self.tabs.selected(),
        );
    }

    fn update_status_item(&mut self, snapshot: &SnapshotData) {
        let style = snapshot.settings.menu_bar;

        // A template image renders monochrome, so severity rides on the title.
        let title = style.shows_percent().then(|| title_text(snapshot));
        self.tray.set_title(title.as_deref());

        if style.shows_icon() != self.icon_shown {
            let icon = style.shows_icon().then(icon::menu_bar_icon);
            let _ = self.tray.set_icon_with_as_template(icon, true);
            self.icon_shown = style.shows_icon();
        }

        let _ = self.tray.set_tooltip(Some(tooltip(snapshot)));
    }

    fn on_menu_event(&mut self, id: &str) {
        let commands = self.controls.on_event(id);
        if !commands.is_empty() {
            for command in commands {
                self.send(command);
            }
            return;
        }
        match id {
            "copy-env" => self.copy_env_snippet(),
            "edit-config" => self.edit_config(),
            "quit" => self.quit(),
            _ => tracing::debug!(id, "unhandled menu event"),
        }
    }

    /// From the **current** snapshot, so the port is the one actually bound, and for the
    /// selected tab: the whole proxy's URLs would hand out access a pool tab withholds.
    fn copy_env_snippet(&self) {
        let snapshot = self.handle.snapshot();
        let pool = match self.tabs.selected() {
            TabId::All => None,
            TabId::Pool(index) => snapshot.pools.get(index),
        };
        let snippet = env::snippet_for(&snapshot.proxy, pool);
        let copied = pasteboard::copy(&snippet);
        self.controls.copy_env().set_text(if copied {
            format!("{COPY_ENV}  ✓")
        } else {
            format!("{COPY_ENV}  (failed)")
        });

        // Revert the acknowledgement without waiting for the next poll.
        self.rt.spawn(async {
            tokio::time::sleep(COPIED_FEEDBACK).await;
            dispatch2::DispatchQueue::main().exec_async(|| {
                UI.with_borrow(|slot| {
                    if let Some(ui) = slot.as_ref() {
                        ui.controls.copy_env().set_text(COPY_ENV);
                    }
                });
            });
        });
    }

    /// Open `config.kdl` in whatever edits text; the engine watches its mtime. **Created
    /// if missing**: `open` fails on a missing path, and a fresh install has no file.
    fn edit_config(&self) {
        let path = libsubby::config::Config::path();
        self.rt.spawn_blocking(move || {
            if !path.exists() {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = std::fs::write(&path, NEW_CONFIG) {
                    tracing::warn!(path = %path.display(), "could not create config.kdl: {e}");
                    return;
                }
            }
            // Nothing is registered for `.kdl` on most machines, so `-t` falls back to
            // the default text editor.
            let opened = std::process::Command::new("open")
                .arg(&path)
                .status()
                .is_ok_and(|s| s.success());
            if opened {
                return;
            }
            match std::process::Command::new("open")
                .arg("-t")
                .arg(&path)
                .status()
            {
                Ok(status) if status.success() => {}
                other => tracing::warn!(
                    path = %path.display(),
                    "could not open config.kdl in a text editor: {other:?}"
                ),
            }
        });
    }

    /// `app.run()` never returns, so quitting is `exit(0)` — after a beat, so a setting
    /// toggled a moment ago still reaches `config.kdl`.
    fn quit(&self) {
        self.send(Command::Shutdown);
        self.rt.spawn(async {
            tokio::time::sleep(QUIT_GRACE).await;
            std::process::exit(0);
        });
    }

    fn send(&self, command: Command) {
        tracing::debug!(?command, "menu");
        self.handle.send(command);
    }
}

/// Append a row to the rebuildable prefix and record which tab owns it. `owners` is
/// index-aligned with the `NSMenu`, so its length **is** the index to insert at.
fn insert(menu: &Menu, owners: &mut Vec<Owner>, item: &dyn IsMenuItem, owner: Owner) -> usize {
    let index = owners.len();
    let _ = menu.insert(item, index);
    owners.push(owner);
    index
}

fn info(text: impl AsRef<str>) -> MenuItem {
    MenuItem::new(text, false, None)
}

/// Append a **drawn** row and hand back its view. The `NSMenuItem` is a disabled
/// placeholder the view covers; AppKit neither performs a view-bearing row's action nor
/// highlights it, which is what makes these rows a readout.
fn insert_row(
    menu: &Menu,
    ns_menu: &objc2_app_kit::NSMenu,
    owners: &mut Vec<Owner>,
    mtm: MainThreadMarker,
    row: subrow::Row,
    owner: Owner,
    tooltip: Option<&str>,
) -> Retained<RowView> {
    let index = insert(menu, owners, &info(""), owner);
    let view = RowView::new(mtm, row);
    if let Some(tooltip) = tooltip {
        // On the view, not the item: AppKit asks whatever owns the rectangle.
        view.setToolTip(Some(&NSString::from_str(tooltip)));
    }
    if let Some(item) = hatch::item_at(ns_menu, index) {
        hatch::set_view(&item, Some(&view));
    }
    view
}

/// The number in the menu bar. The engine computes it, so `subbier status --json` and the
/// bar agree; falls back to the worst enabled window when there is no aggregate yet.
fn overall_pct(snapshot: &SnapshotData) -> Option<f32> {
    snapshot.overall_pct.or_else(|| worst_pct(snapshot))
}

fn worst_pct(snapshot: &SnapshotData) -> Option<f32> {
    snapshot
        .subs
        .iter()
        .filter(|sub| sub.enabled)
        .flat_map(|sub| [sub.session, sub.weekly])
        .flatten()
        .map(|window| window.pct)
        .filter(|pct| pct.is_finite())
        .fold(None, |worst, pct| Some(worst.map_or(pct, |w| w.max(pct))))
}

/// The template icon cannot carry severity, so critical gets a `!` here.
fn title_text(snapshot: &SnapshotData) -> String {
    match overall_pct(snapshot) {
        Some(pct) => {
            let mut title = render::pct(pct);
            if snapshot.worst == Severity::Critical {
                title.push('!');
            }
            title
        }
        None => "—".to_owned(),
    }
}

fn tooltip(snapshot: &SnapshotData) -> String {
    let proxy = match snapshot.proxy.listening {
        Some(addr) if snapshot.proxy.running => format!("subbier — proxying on {addr}"),
        _ if snapshot.settings.proxy_enabled => "subbier — proxy not running".to_owned(),
        _ => "subbier — proxy off".to_owned(),
    };
    // One number over several accounts must say what it averages, or it reads as the worst.
    match (snapshot.overall_pct, snapshot.subs.len()) {
        (Some(pct), n) if n > 1 => format!(
            "{proxy}\n{} of {n} accounts used, weighted by plan size",
            render::pct(pct)
        ),
        _ => proxy,
    }
}

/// The one line of trouble worth a menu row; besides the sub list, the only thing that
/// reshapes the menu.
fn status_line(snapshot: &SnapshotData) -> Option<String> {
    if snapshot.generation == 0 {
        return Some("Starting up…".to_owned());
    }
    if let Some(login) = &snapshot.login {
        return Some(match login {
            libsubby::snapshot::LoginState::AwaitingBrowser { provider, .. } => {
                format!(
                    "Waiting for {} login in your browser…",
                    provider.display_name()
                )
            }
            libsubby::snapshot::LoginState::Failed { provider, error } => {
                format!("{} login failed: {error}", provider.display_name())
            }
        });
    }
    if let Some(error) = &snapshot.last_error {
        return Some(format!("⚠ {error}"));
    }
    if let Some(error) = &snapshot.proxy.last_error {
        return Some(format!("⚠ Proxy: {error}"));
    }
    if snapshot.settings.proxy_enabled && !snapshot.proxy.running {
        return Some("⚠ Proxy is enabled but not running".to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::{SignedDuration, Timestamp};
    use libsubby::CredentialSource;
    use libsubby::snapshot::{ProxyView, RoutingView, SettingsView, SubHealth, WindowView};

    fn window(pct: f32, resets_in: Option<SignedDuration>) -> WindowView {
        WindowView {
            pct,
            resets_at: resets_in.map(|d| Timestamp::now() + d),
            resets_in,
            severity: Severity::Ok,
            projection: None,
        }
    }

    fn sub(id: u32, pct: f32) -> SubView {
        SubView {
            plan_tier: "unknown".into(),
            plan_weight: 1.0,
            id: SubId(id),
            key: libsubby::SubKey::new(Provider::Codex, format!("acct-{id}")),
            provider: Provider::Codex,
            label: "work".to_owned(),
            plan: Some("max20".to_owned()),
            source: CredentialSource::Keychain,
            enabled: true,
            health: SubHealth::Ok,
            session: Some(window(pct, Some(SignedDuration::from_mins(281)))),
            weekly: Some(window(pct / 2.0, None)),
            scoped: Vec::new(),
            routing: RoutingView {
                eligible: true,
                active: true,
                ..RoutingView::default()
            },
        }
    }

    fn snapshot(subs: Vec<SubView>) -> SnapshotData {
        SnapshotData {
            generation: 1,
            subs,
            proxy: ProxyView {
                running: true,
                ..ProxyView::default()
            },
            settings: SettingsView::default(),
            ..SnapshotData::default()
        }
    }

    #[test]
    fn the_title_is_the_worst_enabled_sub() {
        assert_eq!(
            title_text(&snapshot(vec![sub(1, 10.0), sub(2, 92.0)])),
            "92%"
        );

        let mut off = sub(2, 92.0);
        off.enabled = false;
        assert_eq!(title_text(&snapshot(vec![sub(1, 10.0), off])), "10%");
    }

    #[test]
    fn critical_severity_rides_on_the_title_text() {
        let mut snap = snapshot(vec![sub(1, 95.0)]);
        snap.worst = Severity::Critical;
        assert_eq!(title_text(&snap), "95%!");
    }

    #[test]
    fn no_usage_yet_is_not_zero_percent() {
        let mut bare = sub(1, 0.0);
        bare.session = None;
        bare.weekly = None;
        assert_eq!(title_text(&snapshot(vec![bare])), "—");
    }

    #[test]
    fn the_status_row_appears_only_when_something_is_wrong() {
        assert_eq!(status_line(&snapshot(vec![sub(1, 10.0)])), None);
        assert_eq!(
            status_line(&SnapshotData::default()).as_deref(),
            Some("Starting up…")
        );

        let mut dead = snapshot(vec![sub(1, 10.0)]);
        dead.proxy.running = false;
        assert!(status_line(&dead).is_some_and(|s| s.contains("not running")));
    }
}
