//! Tabs: one `NSMenuItem` carries the [`TabStrip`] view, every other row is a stock muda
//! row whose `isHidden` follows the selected tab (`docs/MENU-DESIGN.md` §2). Hiding rows
//! re-lays out an **already open** menu and a view-bearing row performs no action, so a tab
//! switches under the cursor without the menu closing.

pub mod controls;
pub mod hatch;
pub mod rowview;
pub mod subrow;
pub mod tabs;

use libsubby::SubId;
use libsubby::render;
use libsubby::snapshot::{PoolView, SnapshotData};
use muda::Menu;
use objc2::rc::Retained;
use objc2::{AnyThread, define_class, msg_send, sel};
use objc2_app_kit::NSEventTrackingRunLoopMode;
use objc2_foundation::{MainThreadMarker, NSObject, NSRunLoop, NSTimer};

pub use tabs::{Tab, TabId, TabStrip};

/// The tab strip and the separator under it; both vanish when there are no pools.
pub const PREFIX_ROWS: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Owner {
    /// Visible in every tab.
    Common,
    /// A section separator carries the owner of the section it closes, or the menu grows
    /// stacked separators.
    Tab(TabId),
}

impl Owner {
    fn visible_in(self, selected: TabId) -> bool {
        match self {
            Self::Common => true,
            Self::Tab(tab) => tab == selected,
        }
    }
}

pub struct TabBar {
    strip: Retained<TabStrip>,
    /// Index-aligned with the `NSMenu`'s rows: muda reallocates every `NSMenuItem` on
    /// `insert`, so ownership is positional, never by title.
    owners: Vec<Owner>,
    selected: TabId,
}

impl TabBar {
    pub fn new(mtm: MainThreadMarker) -> Self {
        Self {
            strip: TabStrip::new(mtm),
            owners: Vec::new(),
            selected: TabId::All,
        }
    }

    pub fn selected(&self) -> TabId {
        self.selected
    }

    /// `wanted` is honoured when it still exists — a pool disappearing must not throw the
    /// user out of the tab they were reading — else the last tab. Returns whether the rows
    /// now need re-hiding; the strip repaints either way.
    pub fn set_tabs(&mut self, tabs: Vec<Tab>, wanted: TabId) -> bool {
        let selected = tabs
            .iter()
            .position(|tab| tab.id == wanted)
            .or_else(|| tabs.len().checked_sub(1))
            .unwrap_or(0);
        let was = self.selected;
        self.selected = tabs.get(selected).map_or(TabId::All, |tab| tab.id);
        self.strip.set_model(tabs::Model { tabs, selected });
        was != self.selected
    }

    /// Point the map at a freshly built menu and push the state back into AppKit.
    /// **Call this from inside the rebuild**, never after it.
    pub fn apply(&mut self, menu: &Menu, owners: Vec<Owner>) {
        self.owners = owners;
        self.refresh(menu);
    }

    /// Called from `mouseUp:`, inside the menu's tracking loop: nothing here may end it.
    pub fn select(&mut self, menu: &Menu, tab: TabId) {
        if tab == self.selected {
            return;
        }
        let mut model = self.strip.model();
        let Some(index) = model.tabs.iter().position(|t| t.id == tab) else {
            return;
        };
        model.selected = index;
        self.selected = tab;
        self.strip.set_model(model);
        self.refresh(menu);
    }

    /// Re-attach the view and re-apply every `setHidden`. Only valid for the shape
    /// [`TabBar::apply`] was given.
    pub fn refresh(&self, menu: &Menu) {
        let ns_menu = hatch::ns_menu(menu);
        for (index, owner) in self.owners.iter().enumerate() {
            let Some(item) = hatch::item_at(&ns_menu, index) else {
                continue;
            };
            if index == 0 {
                // muda may have replaced the strip's `NSMenuItem` underneath us.
                hatch::set_view(&item, Some(&self.strip));
            }
            // With no pools there is nothing to choose between, so the strip and its
            // separator are hidden rather than drawn empty.
            if index < PREFIX_ROWS && self.strip.model().tabs.is_empty() {
                hatch::set_hidden(&item, true);
                continue;
            }
            hatch::set_hidden(&item, !owner.visible_in(self.selected));
        }
        // Anything past the map (there should be nothing) stays visible, not orphaned hidden.
        for index in self.owners.len()..hatch::len(&ns_menu) {
            if let Some(item) = hatch::item_at(&ns_menu, index) {
                hatch::set_hidden(&item, false);
            }
        }
    }
}

/// The tabs a snapshot deserves, in strip order. **Empty when no pool is configured**: a
/// strip offering one choice costs a row and teaches nothing.
#[must_use]
pub fn available_tabs(snapshot: &SnapshotData) -> Vec<Tab> {
    if snapshot.pools.is_empty() {
        return Vec::new();
    }
    let mut tabs = vec![Tab {
        id: TabId::All,
        name: TabId::All.name(snapshot),
    }];
    tabs.extend(snapshot.pools.iter().enumerate().map(|(index, pool)| Tab {
        id: TabId::Pool(index),
        name: pool.name.clone(),
    }));
    tabs
}

/// A pool tab shows every **member**, including ones a ceiling is holding back: hiding
/// those makes a pool that has stopped routing look like a pool that is empty.
#[must_use]
pub fn tab_shows(snapshot: &SnapshotData, tab: TabId, sub: SubId) -> bool {
    match tab {
        TabId::All => true,
        TabId::Pool(index) => snapshot
            .pools
            .get(index)
            .is_some_and(|pool| pool.members.contains(&sub)),
    }
}

/// `⟳ 3 in flight · 4.1M tok/1h`, or `None` when there is nothing to say. These are
/// **proxy-observed** and a strict subset of what moved the allowance bars, which is what
/// the `⟳` and the `tok/1h` are for.
#[must_use]
pub fn proxy_metrics(in_flight: u32, tokens_1h: u64) -> Option<String> {
    match (in_flight, tokens_1h) {
        (0, 0) => None,
        (0, tokens) => Some(format!("⟳ {} tok/1h", render::tokens(tokens))),
        (n, 0) => Some(format!("⟳ {n} in flight")),
        (n, tokens) => Some(format!(
            "⟳ {n} in flight · {} tok/1h",
            render::tokens(tokens)
        )),
    }
}

/// `session ≤ 50% · weekly ≤ 50%`, or `None` when the pool caps nothing: `100%` is how
/// "no ceiling" is spelled in the snapshot, and `≤ 100%` would read as a limit about to bite.
#[must_use]
pub fn pool_ceilings(pool: &PoolView) -> Option<String> {
    let ceilings: Vec<String> = [
        ("session", pool.max_session_pct),
        ("weekly", pool.max_weekly_pct),
    ]
    .into_iter()
    .filter(|(_, pct)| *pct < 100.0)
    .map(|(name, pct)| format!("{name} ≤ {}", render::pct(pct)))
    .collect();
    (!ceilings.is_empty()).then(|| ceilings.join(" · "))
}

/// Seconds between repaints of an open menu: cheap, but never visibly stale.
const TRACKING_INTERVAL: f64 = 0.4;

define_class!(
    /// The timer's target. It exists only to own a selector.
    #[unsafe(super(NSObject))]
    #[name = "SubbierTracker"]
    struct Tracker;

    impl Tracker {
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: *mut NSTimer) {
            crate::ui::tick_while_tracking();
        }
    }
);

/// Install the timer that repaints a menu **while it is open**. libdispatch does not drain
/// the main queue while a menu tracks, so `DispatchQueue::main()` stops dead the moment the
/// user clicks; a run-loop timer in [`NSEventTrackingRunLoopMode`] still fires.
///
/// **Main thread only.** The timer is deliberately leaked: nothing would cancel it.
pub fn install_tracking_ticker(_mtm: MainThreadMarker) {
    let target: Retained<Tracker> = unsafe { msg_send![Tracker::alloc(), init] };
    // SAFETY: `target` responds to `tick:`, which takes the timer as its one argument.
    let timer = unsafe {
        NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
            TRACKING_INTERVAL,
            &target,
            sel!(tick:),
            None,
            true,
        )
    };
    unsafe {
        NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSEventTrackingRunLoopMode);
    }
    // The timer holds an unretained reference to its target.
    std::mem::forget(target);
}

#[cfg(test)]
mod tests {
    use libsubby::SubId;
    use libsubby::snapshot::PoolView;

    use super::*;

    fn pool(session: f32, weekly: f32) -> PoolView {
        PoolView {
            name: "moonshot".to_owned(),
            provider: None,
            members: vec![SubId(1)],
            eligible: vec![SubId(1)],
            max_session_pct: session,
            max_weekly_pct: weekly,
            openai_base_url: None,
            anthropic_base_url: None,
            proxied_in_flight: 0,
            proxied_tokens_1h: 0,
        }
    }

    #[test]
    fn only_ceilings_that_are_set_are_shown() {
        assert_eq!(
            pool_ceilings(&pool(50.0, 50.0)).as_deref(),
            Some("session ≤ 50% · weekly ≤ 50%")
        );
        assert_eq!(
            pool_ceilings(&pool(100.0, 25.0)).as_deref(),
            Some("weekly ≤ 25%")
        );
        assert_eq!(pool_ceilings(&pool(100.0, 100.0)), None);
    }

    #[test]
    fn the_proxy_metrics_say_they_are_the_proxys_own() {
        assert_eq!(proxy_metrics(0, 0), None);
        assert_eq!(
            proxy_metrics(3, 4_100_000).as_deref(),
            Some("⟳ 3 in flight · 4.1M tok/1h")
        );
        assert_eq!(proxy_metrics(2, 0).as_deref(), Some("⟳ 2 in flight"));
        assert_eq!(proxy_metrics(0, 1200).as_deref(), Some("⟳ 1.2K tok/1h"));
        // Never a percentage: nothing here is an allowance number.
        assert!(!proxy_metrics(3, 4_100_000).unwrap().contains('%'));
    }
}
