//! The middle block: `Proxy ▸` and `Settings ▸`, shown in every tab.
//!
//! A row with an `attributedTitle` silently ignores muda's `set_text` and stops dimming when
//! disabled, so [`Controls::paint`] repaints it positionally every snapshot and hands it back
//! to muda ([`hatch::clear_attributed_title`]) once it has nothing extra to say.

use libsubby::snapshot::{SettingsView, SnapshotData};
use libsubby::{Command, MenuBarStyle, Provider, StrategyKind};
use muda::{CheckMenuItem, IsMenuItem, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSColor, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSMenu,
    NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSTextAlignment, NSTextTab,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSAttributedStringKey, NSDictionary, NSMutableAttributedString,
    NSString,
};

use crate::menu::{Owner, TabId, hatch};

/// The item, the tab that owns it, and the title the hatch restores after a rebuild.
pub struct Row<'a> {
    pub item: &'a dyn IsMenuItem,
    pub owner: Owner,
    /// A proxy toggle: its own title, because muda's is not stable once an attributed
    /// title has been on the row.
    metrics: Option<&'a str>,
}

/// Every control between the sub rows and the trailing block.
pub struct Controls {
    /// A [`Submenu`] rather than a `CheckMenuItem` because muda has no checkable submenu,
    /// so [`hatch::set_state`] draws the checkmark. Turning the proxy **off** is the
    /// `Disabled` entry at the bottom of the strategy list — one list, one answer.
    proxy: Submenu,
    strategy: Picker<StrategyKind>,
    /// `Disabled`, the strategy list's off position.
    disabled: CheckMenuItem,
    /// The per-agent switches, inside the proxy submenu.
    providers: [CheckMenuItem; 2],
    /// `crate::ui` owns the click: copying needs the pasteboard and the selected tab.
    copy_env: MenuItem,
    settings: SettingsBlock,
}

/// `Settings ▸`: the app-level preferences, which are not about routing.
struct SettingsBlock {
    menu: Submenu,
    notifications: CheckMenuItem,
    launch_at_login: CheckMenuItem,
    menu_bar: Picker<MenuBarStyle>,
    /// `Add Codex account` / `Add Claude account`, in [`Provider::ALL`] order.
    add: [MenuItem; 2],
    edit_config: MenuItem,
    refresh: MenuItem,
}

impl Controls {
    pub fn new() -> Self {
        let proxy = Submenu::new(GLOBAL_PROXY, true);
        let strategy = Picker::strategy("strategy");
        let copy_env = MenuItem::with_id("copy-env", COPY_ENV, true, None);
        let disabled = check("proxy:disabled", "Disabled");
        let providers = Provider::ALL.map(|provider| {
            check(
                format!("settings:proxy:{}", provider.id()),
                provider.display_name(),
            )
        });
        // Strategies are flattened in rather than nested: a second disclosure is two
        // clicks for the most-used control.
        let _ = proxy.append(&copy_env);
        let _ = proxy.append(&PredefinedMenuItem::separator());
        for item in strategy.entries() {
            let _ = proxy.append(item);
        }
        let _ = proxy.append(&PredefinedMenuItem::separator());
        for item in &providers {
            let _ = proxy.append(item);
        }
        let _ = proxy.append(&PredefinedMenuItem::separator());
        let _ = proxy.append(&disabled);

        Self {
            proxy,
            strategy,
            disabled,
            providers,
            copy_env,
            settings: SettingsBlock::new(),
        }
    }

    /// For the one thing `crate::ui` does to it: flash an acknowledgement after a copy.
    pub fn copy_env(&self) -> &MenuItem {
        &self.copy_env
    }

    /// Every row, in menu order — [`Controls::paint`] walks the same list.
    pub fn rows(&self) -> Vec<Row<'_>> {
        vec![
            Row {
                item: &self.proxy,
                owner: Owner::Common,
                metrics: Some(GLOBAL_PROXY),
            },
            Row {
                item: &self.settings.menu,
                owner: Owner::Common,
                metrics: None,
            },
        ]
    }

    /// Every checkmark and enabled state; the metrics and the proxy row's own checkmark
    /// are [`Controls::paint`]'s.
    pub fn sync(&self, snapshot: &SnapshotData) {
        let settings = &snapshot.settings;
        // `Lowest usage` ticked under a `Disabled` proxy says the opposite of the truth.
        if settings.proxy_enabled {
            self.strategy.sync(settings.strategy);
        } else {
            self.strategy.clear();
        }
        self.disabled.set_checked(!settings.proxy_enabled);
        for (item, provider) in self.providers.iter().zip(Provider::ALL) {
            item.set_checked(settings.proxies(provider));
            item.set_enabled(settings.proxy_enabled);
        }
        self.settings.sync(settings);
    }

    /// Re-apply everything AppKit-side: the metrics title, and the checkmark on a submenu
    /// row muda cannot check for us. `base` moves whenever the prefix above is rebuilt,
    /// hence a position rather than a cached `NSMenuItem`. **Main thread only.**
    pub fn paint(&self, menu: &NSMenu, base: usize, snapshot: &SnapshotData, tab: TabId) {
        for (slot, row) in self.rows().iter().enumerate() {
            let Some(item) = hatch::item_at(menu, base + slot) else {
                continue;
            };
            let Some(title) = row.metrics else {
                continue;
            };
            hatch::set_state(&item, snapshot.settings.proxy_enabled);
            match proxy_suffix(snapshot, tab) {
                // Nothing to add: muda draws highlight and dimming better than we can.
                None => hatch::clear_attributed_title(&item, title),
                Some(suffix) => {
                    hatch::set_attributed_title(&item, &toggle_title(title, &suffix));
                }
            }
        }
    }

    /// The commands a click on `id` means, or empty if the row is not ours. A
    /// `CheckMenuItem` has already flipped itself, so its new state **is** the intent.
    pub fn on_event(&self, id: &str) -> Vec<Command> {
        if is(&self.disabled, id) {
            // Off whichever way the checkbox went: re-clicking a checked `Disabled`
            // would otherwise read as "turn the proxy back on".
            return vec![Command::SetProxyEnabled(false)];
        }
        if let Some(kind) = self.strategy.on_event(id) {
            // Choosing a strategy on a stopped proxy starts it: the list is one control.
            self.disabled.set_checked(false);
            return vec![Command::SetStrategy(kind), Command::SetProxyEnabled(true)];
        }
        for (item, provider) in self.providers.iter().zip(Provider::ALL) {
            if is(item, id) {
                return vec![Command::SetProviderProxied {
                    provider,
                    on: item.is_checked(),
                }];
            }
        }
        self.settings.on_event(id).into_iter().collect()
    }
}

impl SettingsBlock {
    fn new() -> Self {
        let menu = Submenu::new("Settings", true);
        let notifications = check("notifications", "Notifications");
        let launch_at_login = check("launch-at-login", "Launch at login");
        let menu_bar = Picker::new(
            "Menu bar",
            "menu-bar",
            MenuBarStyle::ALL,
            menu_bar_label,
            MenuBarStyle::as_str,
        );
        let add = Provider::ALL.map(|provider| {
            MenuItem::with_id(
                format!("login:{}", provider.id()),
                format!("Add {} account…", provider.display_name()),
                true,
                None,
            )
        });
        // Not a Command: `crate::ui` matches this id after the block declines it.
        let edit_config = MenuItem::with_id("edit-config", "Edit config.kdl…", true, None);
        let refresh = MenuItem::with_id("refresh", "Refresh now", true, None);

        let _ = menu.append(&notifications);
        let _ = menu.append(&launch_at_login);
        let _ = menu.append(menu_bar.menu());
        let _ = menu.append(&PredefinedMenuItem::separator());
        for item in &add {
            let _ = menu.append(item);
        }
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&edit_config);
        let _ = menu.append(&refresh);

        Self {
            menu,
            notifications,
            launch_at_login,
            menu_bar,
            add,
            edit_config,
            refresh,
        }
    }

    fn sync(&self, settings: &SettingsView) {
        self.menu_bar.sync(settings.menu_bar);
        self.notifications
            .set_checked(settings.notifications_enabled);
        self.launch_at_login.set_checked(settings.launch_at_login);
    }

    fn on_event(&self, id: &str) -> Option<Command> {
        if let Some(style) = self.menu_bar.on_event(id) {
            return Some(Command::SetMenuBar(style));
        }
        for (item, provider) in self.add.iter().zip(Provider::ALL) {
            if item.id() == &MenuId::from(id) {
                return Some(Command::Login(provider));
            }
        }
        if self.edit_config.id() == &MenuId::from(id) {
            return None;
        }
        if is(&self.notifications, id) {
            return Some(Command::SetNotificationsEnabled(
                self.notifications.is_checked(),
            ));
        }
        if is(&self.launch_at_login, id) {
            return Some(Command::SetLaunchAtLogin(self.launch_at_login.is_checked()));
        }
        if self.refresh.id() == &MenuId::from(id) {
            return Some(Command::RefreshUsage { force: true });
        }
        None
    }
}

/// A `Submenu` of check items of which exactly one is checked: muda has no radio item,
/// so exclusivity is enforced by hand — here, once.
struct Picker<T> {
    menu: Submenu,
    items: Vec<(T, CheckMenuItem)>,
}

impl<T: Copy + PartialEq> Picker<T> {
    fn new(
        title: &str,
        id_prefix: impl std::fmt::Display,
        all: impl IntoIterator<Item = T>,
        label: impl Fn(T) -> &'static str,
        key: impl Fn(T) -> &'static str,
    ) -> Self {
        let menu = Submenu::new(title, true);
        let items: Vec<(T, CheckMenuItem)> = all
            .into_iter()
            .map(|value| {
                let item = check(format!("{id_prefix}:{}", key(value)), label(value));
                let _ = menu.append(&item);
                (value, item)
            })
            .collect();
        Self { menu, items }
    }

    fn menu(&self) -> &Submenu {
        &self.menu
    }

    /// For a caller that flattens them in rather than nesting behind [`Picker::menu`].
    fn entries(&self) -> impl Iterator<Item = &CheckMenuItem> {
        self.items.iter().map(|(_, item)| item)
    }

    /// For a picker whose choice is not in force — a strategy list under a stopped proxy.
    fn clear(&self) {
        for (_, item) in &self.items {
            item.set_checked(false);
        }
    }

    fn sync(&self, current: T) {
        for (value, item) in &self.items {
            item.set_checked(*value == current);
        }
    }

    /// The choice `id` names, having already unchecked its siblings — the click did not.
    fn on_event(&self, id: &str) -> Option<T> {
        let chosen = self
            .items
            .iter()
            .find_map(|(value, item)| is(item, id).then_some(*value))?;
        self.sync(chosen);
        Some(chosen)
    }
}

impl Picker<StrategyKind> {
    fn strategy(id_prefix: impl std::fmt::Display) -> Self {
        Self::new(
            "Strategy",
            id_prefix,
            StrategyKind::ALL,
            strategy_label,
            StrategyKind::as_str,
        )
    }
}

const GLOBAL_PROXY: &str = "Proxy";

/// Also what `crate::ui` reverts the row to after flashing an acknowledgement.
pub const COPY_ENV: &str = "Copy env snippet";

/// The dim half of the `Proxy ▸` row: `⟳ 1 in flight · 90.0K tok/1h · session ≤ 50%`.
///
/// A pool's numbers are its **own endpoint's**, never summed over members that also serve
/// other pools. Its ceilings show while idle: they answer "why is this pool not using that
/// account".
fn proxy_suffix(snapshot: &SnapshotData, tab: TabId) -> Option<String> {
    let pool = match tab {
        TabId::All => None,
        TabId::Pool(index) => snapshot.pools.get(index),
    };
    let (in_flight, tokens_1h) = match pool {
        None => (
            snapshot.proxy.proxied_in_flight,
            snapshot.proxy.proxied_tokens_1h,
        ),
        Some(pool) => (pool.proxied_in_flight, pool.proxied_tokens_1h),
    };
    let parts: Vec<String> = crate::menu::proxy_metrics(in_flight, tokens_1h)
        .into_iter()
        .chain(pool.and_then(crate::menu::pool_ceilings))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The sub rows' right edge, less a submenu arrow's worth: AppKit adds the ▸ beyond a tab
/// stop, so aligning at the full row width makes the menu wider than its own rows.
const METRICS_EDGE: f64 = crate::menu::rowview::ROW_WIDTH - 30.0;

/// The proxy row as one attributed string: title, then metrics right-aligned to where the
/// sub rows end — a tab stop rather than padding, this being a proportional font.
fn toggle_title(title: &str, suffix: &str) -> Retained<NSAttributedString> {
    let font = NSFont::menuFontOfSize(0.0);
    let out = NSMutableAttributedString::alloc();
    let out = NSMutableAttributedString::initWithString(out, &NSString::from_str(""));
    out.appendAttributedString(&run(title, &font, &NSColor::labelColor(), None));

    let small = NSFont::menuFontOfSize(font.pointSize() - 1.0);
    // A metric must never outweigh the switch it is attached to.
    let color = NSColor::secondaryLabelColor();
    out.appendAttributedString(&run(
        &format!("\t{suffix}"),
        &small,
        &color,
        Some(METRICS_EDGE),
    ));
    out.into_super()
}

/// One attribute run. `right_edge` gives it the tab stop the metrics hang off.
fn run(
    text: &str,
    font: &NSFont,
    color: &NSColor,
    right_edge: Option<f64>,
) -> Retained<NSAttributedString> {
    // SAFETY: all three are the documented attribute-name statics.
    let (font_key, color_key, style_key) = unsafe {
        (
            NSFontAttributeName,
            NSForegroundColorAttributeName,
            NSParagraphStyleAttributeName,
        )
    };
    let mut keys: Vec<&NSAttributedStringKey> = vec![font_key, color_key];
    let mut values: Vec<&AnyObject> = vec![as_object(font), as_object(color)];
    let style = right_edge.map(right_aligned_at);
    if let Some(style) = &style {
        keys.push(style_key);
        values.push(as_object(&**style));
    }
    let attributes = NSDictionary::<NSAttributedStringKey, AnyObject>::from_slices(&keys, &values);
    // SAFETY: `attributes` is the documented `NSAttributedStringKey -> id` dictionary.
    unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(text),
            Some(&attributes),
        )
    }
}

/// An attribute value as the `id` the dictionary wants.
fn as_object<T>(value: &T) -> &AnyObject {
    // SAFETY: every caller passes an Objective-C object reference outliving the dictionary.
    unsafe { &*std::ptr::from_ref(value).cast::<AnyObject>() }
}

/// A paragraph style whose one tab stop right-aligns at `location`.
fn right_aligned_at(location: f64) -> Retained<NSMutableParagraphStyle> {
    let style = NSMutableParagraphStyle::new();
    let stop = NSTextTab::alloc();
    // SAFETY: the designated initialiser; an empty options dictionary means no terminators.
    let stop = unsafe {
        NSTextTab::initWithTextAlignment_location_options(
            stop,
            NSTextAlignment::Right,
            location,
            &NSDictionary::new(),
        )
    };
    style.setTabStops(Some(&NSArray::from_slice(&[&*stop])));
    style
}

fn check(id: impl Into<MenuId>, text: impl AsRef<str>) -> CheckMenuItem {
    CheckMenuItem::with_id(id, text, true, false, None)
}

fn is(item: &CheckMenuItem, id: &str) -> bool {
    item.id().as_ref() == id
}

const fn strategy_label(kind: StrategyKind) -> &'static str {
    match kind {
        StrategyKind::LowestUsage => "Lowest usage",
        StrategyKind::HighestUsage => "Highest usage",
        StrategyKind::RoundRobin => "Round robin",
        StrategyKind::LeastConnections => "Least connections",
    }
}

const fn menu_bar_label(style: MenuBarStyle) -> &'static str {
    match style {
        MenuBarStyle::IconPercent => "Mark and percentage",
        MenuBarStyle::Icon => "Mark only",
        MenuBarStyle::Percent => "Percentage only",
    }
}

#[cfg(test)]
mod tests {
    use libsubby::SubId;
    use libsubby::snapshot::{PoolView, ProxyView};

    use super::*;

    fn pool(in_flight: u32, tokens: u64, session: f32, weekly: f32) -> PoolView {
        PoolView {
            name: "moonshot".to_owned(),
            provider: None,
            members: vec![SubId(1)],
            eligible: vec![SubId(1)],
            max_session_pct: session,
            max_weekly_pct: weekly,
            openai_base_url: None,
            anthropic_base_url: None,
            proxied_in_flight: in_flight,
            proxied_tokens_1h: tokens,
        }
    }

    fn snapshot(in_flight: u32, tokens: u64, pools: Vec<PoolView>) -> SnapshotData {
        SnapshotData {
            generation: 1,
            pools,
            proxy: ProxyView {
                running: true,
                proxied_in_flight: in_flight,
                proxied_tokens_1h: tokens,
                ..ProxyView::default()
            },
            ..SnapshotData::default()
        }
    }

    #[test]
    fn the_all_tab_says_what_the_whole_proxy_is_carrying() {
        let snap = snapshot(4, 4_100_000, Vec::new());
        assert_eq!(
            proxy_suffix(&snap, TabId::All).as_deref(),
            Some("⟳ 4 in flight · 4.1M tok/1h")
        );
    }

    #[test]
    fn a_pool_tab_says_that_pools_traffic_and_that_pools_ceilings() {
        let snap = snapshot(9, 9_000_000, vec![pool(1, 90_000, 50.0, 50.0)]);
        assert_eq!(
            proxy_suffix(&snap, TabId::Pool(0)).as_deref(),
            Some("⟳ 1 in flight · 90.0K tok/1h · session ≤ 50% · weekly ≤ 50%"),
            "a pool's numbers are its own, not the whole proxy's"
        );
    }

    #[test]
    fn an_idle_endpoint_says_only_its_ceilings() {
        let capped = snapshot(0, 0, vec![pool(0, 0, 100.0, 25.0)]);
        assert_eq!(
            proxy_suffix(&capped, TabId::Pool(0)).as_deref(),
            Some("weekly ≤ 25%")
        );

        let uncapped = snapshot(0, 0, vec![pool(0, 0, 100.0, 100.0)]);
        assert_eq!(proxy_suffix(&uncapped, TabId::All), None);
        assert_eq!(proxy_suffix(&uncapped, TabId::Pool(0)), None);
    }

    #[test]
    fn a_pool_index_past_the_end_falls_back_to_the_whole_proxy() {
        let snap = snapshot(2, 0, Vec::new());
        assert_eq!(
            proxy_suffix(&snap, TabId::Pool(7)).as_deref(),
            Some("⟳ 2 in flight")
        );
    }
}
