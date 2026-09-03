//! The tab strip: one `NSView` on the first row of the menu.
//!
//! Semantic `NSColor`s follow light/dark and the accent colour for free, so nothing below
//! branches on appearance. Geometry comes from [`NSView::bounds`], never a width constant:
//! the menu is as wide as its widest row.

use std::cell::RefCell;

use libsubby::snapshot::SnapshotData;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBezierPath, NSColor, NSEvent, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSStringDrawing, NSView,
};
use objc2_foundation::{MainThreadMarker, NSDictionary, NSPoint, NSRect, NSSize, NSString};

/// [`TabId::Pool`] holds an index rather than a name so the id is `Copy` for the
/// [`crate::menu::Owner`] map; the strip is rebuilt whenever the pool list changes, so the
/// index cannot go stale between a rebuild and a click.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabId {
    /// Every account, whatever pool it is in. Always first.
    All,
    /// One configured pool, by index into `SnapshotData::pools`.
    Pool(usize),
}

impl TabId {
    #[must_use]
    pub fn name(self, snapshot: &SnapshotData) -> String {
        match self {
            Self::All => "All subs".to_owned(),
            Self::Pool(index) => snapshot
                .pools
                .get(index)
                .map_or_else(|| format!("Pool {index}"), |p| p.name.clone()),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Tab {
    pub id: TabId,
    /// Resolved here rather than at draw time, so the strip repaints with no snapshot.
    pub name: String,
}

#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Model {
    pub tabs: Vec<Tab>,
    /// Index into `tabs`; out of range draws nothing selected, as an empty strip should.
    pub selected: usize,
}

/// `NSMenuItem` sizes itself to the view's frame.
pub const HEIGHT: f64 = 30.0;

/// A view-bearing row owns the checkmark gutter, so a stock row's text inset is ours.
const LEFT_INSET: f64 = 21.0;

const PILL_PAD_X: f64 = 9.0;

const GAP: f64 = 6.0;

const FONT_SIZE: f64 = 13.0;

/// Meaningless after the first layout, but AppKit wants a frame to start from.
const INITIAL_WIDTH: f64 = 320.0;

pub struct StripIvars {
    model: RefCell<Model>,
}

define_class!(
    /// `#[thread_kind = MainThreadOnly]` makes the main-thread rule a compile error.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "SubbierTabStrip"]
    #[ivars = StripIvars]
    pub struct TabStrip;

    impl TabStrip {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        /// The click that must not dismiss the menu: no `cancelTracking`, and no
        /// `muda::MenuEvent` — AppKit never performs a view-bearing row's action
        /// (`docs/MENU-DESIGN.md` §2).
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let in_window = event.locationInWindow();
            let point = self.convertPoint_fromView(in_window, None);
            let Some(tab) = self.hit_test_tab(point) else {
                return;
            };
            crate::ui::select_tab(tab);
        }
    }
);

impl TabStrip {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(StripIvars {
            model: RefCell::new(Model::default()),
        });
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(INITIAL_WIDTH, HEIGHT));
        // SAFETY: the superclass's designated initialiser, called once on a fresh alloc.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        // Without this the view is stranded at `INITIAL_WIDTH` as the menu grows.
        this.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        this
    }

    pub fn set_model(&self, model: Model) {
        let changed = *self.ivars().model.borrow() != model;
        if changed {
            *self.ivars().model.borrow_mut() = model;
            self.setNeedsDisplay(true);
        }
    }

    pub fn model(&self) -> Model {
        self.ivars().model.borrow().clone()
    }

    /// Pill rectangles, left to right, in the view's own coordinates.
    fn layout(&self) -> Vec<NSRect> {
        let bounds = self.bounds();
        let font = menu_font();
        let mut x = LEFT_INSET;
        let mut rects = Vec::new();
        for tab in &self.ivars().model.borrow().tabs {
            let width = text_size(&tab.name, &font).width + PILL_PAD_X * 2.0;
            rects.push(NSRect::new(
                NSPoint::new(x, 2.0),
                NSSize::new(width, bounds.size.height - 4.0),
            ));
            x += width + GAP;
        }
        rects
    }

    /// Half the gap on either side counts, so there is no dead pixel between two pills.
    fn hit_test_tab(&self, point: NSPoint) -> Option<TabId> {
        let bounds = self.bounds();
        if point.y < 0.0 || point.y > bounds.size.height {
            return None;
        }
        let model = self.ivars().model.borrow();
        self.layout()
            .into_iter()
            .zip(&model.tabs)
            .find(|(rect, _)| {
                point.x >= rect.origin.x - GAP / 2.0
                    && point.x <= rect.origin.x + rect.size.width + GAP / 2.0
            })
            .map(|(_, tab)| tab.id)
    }

    fn draw(&self) {
        let model = self.ivars().model.borrow();
        let font = menu_font();
        for (index, (rect, tab)) in self.layout().into_iter().zip(&model.tabs).enumerate() {
            let selected = index == model.selected;
            let color = if selected {
                NSColor::selectedContentBackgroundColor().setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, 6.0, 6.0).fill();
                NSColor::selectedMenuItemTextColor()
            } else {
                NSColor::secondaryLabelColor()
            };
            let size = text_size(&tab.name, &font);
            draw_text(
                &tab.name,
                NSPoint::new(
                    rect.origin.x + (rect.size.width - size.width) / 2.0,
                    rect.origin.y + (rect.size.height - size.height) / 2.0,
                ),
                &font,
                &color,
            );
        }
    }
}

fn menu_font() -> Retained<NSFont> {
    NSFont::menuFontOfSize(FONT_SIZE)
}

fn attributes(font: &NSFont, color: &NSColor) -> Retained<NSDictionary<NSString, AnyObject>> {
    // SAFETY: documented attribute names, with a value of the type each key requires.
    unsafe {
        NSDictionary::from_slices(
            &[NSFontAttributeName, NSForegroundColorAttributeName],
            &[
                &*std::ptr::from_ref(font).cast::<AnyObject>(),
                &*std::ptr::from_ref(color).cast::<AnyObject>(),
            ],
        )
    }
}

fn draw_text(text: &str, at: NSPoint, font: &NSFont, color: &NSColor) {
    let string = NSString::from_str(text);
    // SAFETY: inside `drawRect:`, so there is a current graphics context.
    unsafe { string.drawAtPoint_withAttributes(at, Some(&attributes(font, color))) };
}

fn text_size(text: &str, font: &NSFont) -> NSSize {
    let string = NSString::from_str(text);
    let color = NSColor::labelColor();
    // SAFETY: measurement only; no graphics context required.
    unsafe { string.sizeWithAttributes(Some(&attributes(font, &color))) }
}
