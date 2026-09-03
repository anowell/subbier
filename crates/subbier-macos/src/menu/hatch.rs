//! The escape hatch: muda's `NSMenu` and the `NSMenuItem`s it built, so every row is
//! reachable with the full AppKit surface. Every `unsafe` the menu needs lives here, on the
//! main thread — a `&muda::Menu` is proof of it. Nothing applied here survives a rebuild:
//! muda allocates a *fresh* `NSMenuItem` per `insert`, so re-apply by index, never by title.

use muda::{ContextMenu, Menu};
use objc2::rc::Retained;
use objc2_app_kit::{NSControlStateValueOff, NSControlStateValueOn, NSMenu, NSMenuItem, NSView};
use objc2_foundation::{NSAttributedString, NSString};

/// Retain the `NSMenu` muda is driving. **Main thread only.** Panics if muda hands back a
/// null menu: the `Menu` was never realised, so there is no menu bar app to continue into.
pub fn ns_menu(menu: &Menu) -> Retained<NSMenu> {
    let ptr = menu.ns_menu().cast::<NSMenu>();
    // SAFETY: muda's own `NSMenu`, valid while the `Menu` is; `retain` takes our own +1.
    unsafe { Retained::retain(ptr) }.expect("muda handed back a null NSMenu")
}

pub fn item_at(menu: &NSMenu, index: usize) -> Option<Retained<NSMenuItem>> {
    let index = isize::try_from(index).ok()?;
    if index >= menu.numberOfItems() {
        return None;
    }
    menu.itemAtIndex(index)
}

pub fn len(menu: &NSMenu) -> usize {
    usize::try_from(menu.numberOfItems()).unwrap_or(0)
}

/// The view then owns the row's whole rectangle, including the checkmark gutter, and
/// AppKit stops performing the row's action — clicks go to the view instead.
pub fn set_view(item: &NSMenuItem, view: Option<&NSView>) {
    item.setView(view);
}

/// Re-lays out a menu that is **already open**, which is what makes the tabs work.
pub fn set_hidden(item: &NSMenuItem, hidden: bool) {
    item.setHidden(hidden);
}

/// **A one-way door**: from here on `muda::MenuItem::set_text` on this row is a *silent
/// visual no-op*, and AppKit's own dimming of a disabled row is overridden.
pub fn set_attributed_title(item: &NSMenuItem, title: &NSAttributedString) {
    item.setAttributedTitle(Some(title));
}

/// Hand a row back to muda, so AppKit draws its own highlight and dimming again. `title`
/// is passed rather than read back: `setAttributedTitle` also writes `title` on some
/// AppKit versions.
pub fn clear_attributed_title(item: &NSMenuItem, title: &str) {
    item.setAttributedTitle(None);
    item.setTitle(&NSString::from_str(title));
}

/// A checkmark on a row muda has no checkbox for: a `Submenu` cannot be a `CheckMenuItem`,
/// but its `NSMenuItem` still has a `state`. **Main thread only.**
pub fn set_state(item: &NSMenuItem, on: bool) {
    let state = if on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    };
    item.setState(state);
}
