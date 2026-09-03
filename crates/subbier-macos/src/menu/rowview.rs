//! A row of the block, **drawn**: one `NSView` per row, one `drawRect:`.
//!
//! A view rather than an `NSAttributedString`: finer bars, no AppKit dimming of a disabled
//! row, and no action or highlight on a view-bearing row, so the menu stays open.
//!
//! ```text
//!  ·  anthony@howie.ai   Max 20x   ▬▬▬▬▬▭▭▭  67% (4h)   ▬▬▭▭▭▭▭▭  31%
//!  ↑  └ label 150 ──────┘└ plan ─┘ └ bar 64┘ └pct┘└rst┘
//!  dot: routing here right now
//! ```

use std::cell::RefCell;

use libsubby::Severity;
use libsubby::render;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAppearance, NSAutoresizingMaskOptions, NSBezierPath, NSColor, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSStringDrawing, NSView,
};
use objc2_foundation::{MainThreadMarker, NSDictionary, NSPoint, NSRect, NSSize, NSString};

use crate::menu::subrow::{Meter, Row, SubRow};

/// A view-bearing row owns the checkmark gutter, so a stock row's text inset is ours.
const LEFT: f64 = 21.0;
/// Trailing padding, so nothing touches the menu's right edge.
const RIGHT_PAD: f64 = 13.0;

const LABEL_W: f64 = 150.0;
const PLAN_X: f64 = LEFT + LABEL_W + 8.0;
const PLAN_W: f64 = 74.0;

/// Wide enough that a percentage point is a visible step.
const BAR_W: f64 = 64.0;
const BAR_H: f64 = 6.0;
/// A percentage, right-aligned in its own column. `100%` is the widest value.
const PCT_W: f64 = 36.0;
/// ` (23h)` — the widest countdown a 5h session or a 7d week can produce.
const RESET_W: f64 = 38.0;
const GAP: f64 = 8.0;
/// The narrowest a non-zero bar may draw: visible, but 2% and 12% still differ.
const MIN_FILL: f64 = 2.0;

const SESSION_X: f64 = PLAN_X + PLAN_W + 8.0;
/// The right edge of the session percentage — what `SESSION` is aligned to.
const SESSION_PCT_R: f64 = SESSION_X + BAR_W + GAP + PCT_W;
const SESSION_RESET_X: f64 = SESSION_PCT_R + 6.0;
const WEEKLY_X: f64 = SESSION_RESET_X + RESET_W + 10.0;
const WEEKLY_PCT_R: f64 = WEEKLY_X + BAR_W + GAP + PCT_W;
const WEEKLY_RESET_X: f64 = WEEKLY_PCT_R + 6.0;

/// The menu is as wide as its widest row, so this **is** the menu's width.
pub const ROW_WIDTH: f64 = WEEKLY_RESET_X + RESET_W + RIGHT_PAD;

/// The dot that marks the account the proxy is routing to right now.
const DOT_R: f64 = 3.5;
const DOT_CX: f64 = 10.5;

const TEXT_SIZE: f64 = 13.0;
const SMALL_SIZE: f64 = 11.0;

pub struct RowIvars {
    row: RefCell<Row>,
}

define_class!(
    /// `#[thread_kind = MainThreadOnly]` makes the main-thread rule a compile error.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "SubbierRowView"]
    #[ivars = RowIvars]
    pub struct RowView;

    impl RowView {
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }
    }
);

impl RowView {
    /// A view for one row, already sized. **Main thread only.**
    pub fn new(mtm: MainThreadMarker, row: Row) -> Retained<Self> {
        let height = row.height();
        let this = Self::alloc(mtm).set_ivars(RowIvars {
            row: RefCell::new(row),
        });
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(ROW_WIDTH, height));
        // SAFETY: the superclass's designated initialiser, called once on a fresh alloc.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        // A submenu arrow on the rows below makes the menu wider than `ROW_WIDTH`; without
        // this the rules stop short of an edge every stock separator reaches.
        this.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        this
    }

    /// Only when something changed, which matters at four snapshots a second.
    pub fn set_row(&self, row: Row) {
        if *self.ivars().row.borrow() != row {
            *self.ivars().row.borrow_mut() = row;
            self.setNeedsDisplay(true);
        }
    }

    fn draw(&self) {
        let row = self.ivars().row.borrow().clone();
        let bounds = self.bounds();
        let dark = is_dark();
        match &row {
            Row::Columns => self.draw_columns(bounds),
            Row::Section { name } => self.draw_section(bounds, name),
            Row::Sub(sub) => self.draw_sub(bounds, sub, dark),
        }
    }

    /// Each header right-aligned to the percentage it names, not the countdown beside it.
    fn draw_columns(&self, bounds: NSRect) {
        let font = small();
        let color = tertiary();
        for (text, right) in [("SESSION", SESSION_PCT_R), ("WEEKLY", WEEKLY_PCT_R)] {
            let width = text_size(text, &font).width;
            draw_text(
                text,
                right - width,
                mid_y(bounds, SMALL_SIZE),
                &font,
                &color,
            );
        }
    }

    /// The name, then a hairline to the right edge: this row's job is to **divide**.
    fn draw_section(&self, bounds: NSRect, name: &str) {
        let font = semibold();
        let name_w = text_size(name, &font).width;
        // The column headers' grey: a section name labels the rows under it.
        draw_text(name, LEFT, mid_y(bounds, TEXT_SIZE), &font, &tertiary());

        // From `bounds`, not `ROW_WIDTH`: the row stretches to whatever the menu became.
        let rule_end = bounds.size.width - RIGHT_PAD;
        let rule_start = LEFT + name_w + GAP;
        if rule_end > rule_start {
            let y = (bounds.size.height / 2.0).floor();
            hairline().setFill();
            NSBezierPath::fillRect(NSRect::new(
                NSPoint::new(rule_start, y),
                NSSize::new(rule_end - rule_start, 1.0),
            ));
        }
    }

    fn draw_sub(&self, bounds: NSRect, sub: &SubRow, dark: bool) {
        if sub.active {
            NSColor::controlAccentColor().setFill();
            let dot = NSRect::new(
                NSPoint::new(DOT_CX - DOT_R, bounds.size.height / 2.0 - DOT_R),
                NSSize::new(DOT_R * 2.0, DOT_R * 2.0),
            );
            NSBezierPath::bezierPathWithOvalInRect(dot).fill();
        }

        let font = text();
        let y = mid_y(bounds, TEXT_SIZE);
        // Full strength always: an account that is off says so in the next column.
        draw_clipped(&sub.label, LEFT, y, LABEL_W, &font, &label());

        let small = small();
        match &sub.flag {
            Some(flag) => draw_clipped(
                flag,
                PLAN_X,
                mid_y(bounds, SMALL_SIZE),
                PLAN_W,
                &small,
                &band(Severity::Warn, dark),
            ),
            None => draw_clipped(
                &sub.plan,
                PLAN_X,
                mid_y(bounds, SMALL_SIZE),
                PLAN_W,
                &small,
                &secondary(),
            ),
        }

        self.draw_meter(bounds, sub.session.as_ref(), SESSION_X, dark);
        self.draw_meter(bounds, sub.weekly.as_ref(), WEEKLY_X, dark);
    }

    /// One bar, its percentage and its countdown — or `—` and **no track at all** for a
    /// window never reported: an empty track reads as a real 0%.
    fn draw_meter(&self, bounds: NSRect, meter: Option<&Meter>, x: f64, dark: bool) {
        let (pct_right, reset_x) = if x == SESSION_X {
            (SESSION_PCT_R, SESSION_RESET_X)
        } else {
            (WEEKLY_PCT_R, WEEKLY_RESET_X)
        };
        let font = digits();
        let Some(meter) = meter else {
            let width = text_size("—", &font).width;
            draw_text(
                "—",
                pct_right - width,
                mid_y(bounds, SMALL_SIZE),
                &font,
                &tertiary(),
            );
            return;
        };

        let y = (bounds.size.height / 2.0 - BAR_H / 2.0).floor();
        let track = NSRect::new(NSPoint::new(x, y), NSSize::new(BAR_W, BAR_H));
        track_color(dark).setFill();
        rounded(track).fill();

        let filled = fill_width(meter.pct, BAR_W);
        if filled > 0.0 {
            band(meter.severity, dark).setFill();
            rounded(NSRect::new(NSPoint::new(x, y), NSSize::new(filled, BAR_H))).fill();
        }

        let text = render::pct(meter.pct);
        let width = text_size(&text, &font).width;
        draw_text(
            &text,
            pct_right - width,
            mid_y(bounds, SMALL_SIZE),
            &font,
            &band(meter.severity, dark),
        );

        if !meter.reset.is_empty() {
            // Never the band colour: red beside a red bar reads as a second warning.
            draw_text(
                &meter.reset,
                reset_x,
                mid_y(bounds, SMALL_SIZE),
                &small(),
                &tertiary(),
            );
        }
    }
}

/// How much of a bar a percentage fills, in points. **A non-zero percentage never draws as
/// empty**: anything above zero gets at least [`MIN_FILL`].
fn fill_width(pct: f32, width: f64) -> f64 {
    if pct <= 0.0 {
        return 0.0;
    }
    let exact = f64::from(pct.clamp(0.0, 100.0)) / 100.0 * width;
    exact.max(MIN_FILL).min(width)
}

fn rounded(rect: NSRect) -> Retained<NSBezierPath> {
    let radius = rect.size.height / 2.0;
    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(rect, radius, radius)
}

/// Read inside `drawRect:`, where the row's appearance is already current — so a
/// system-wide switch repaints into the other palette with nothing to observe.
fn is_dark() -> bool {
    NSAppearance::currentDrawingAppearance()
        .name()
        .to_string()
        .contains("Dark")
}

/// **Not `systemGreenColor` and friends**: on a light menu, system green on grey is
/// close to unreadable.
fn band(severity: Severity, dark: bool) -> Retained<NSColor> {
    let (r, g, b) = match (severity, dark) {
        (Severity::Ok, false) => (0.11, 0.51, 0.24),
        (Severity::Ok, true) => (0.35, 0.83, 0.49),
        (Severity::Warn, false) => (0.70, 0.37, 0.02),
        (Severity::Warn, true) => (1.00, 0.72, 0.25),
        (Severity::Critical, false) => (0.75, 0.16, 0.12),
        (Severity::Critical, true) => (1.00, 0.45, 0.42),
    };
    NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0)
}

/// Present enough to show what is left, quiet enough not to read as usage.
fn track_color(dark: bool) -> Retained<NSColor> {
    if dark {
        NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 1.0, 1.0, 0.17)
    } else {
        NSColor::colorWithSRGBRed_green_blue_alpha(0.0, 0.0, 0.0, 0.11)
    }
}

fn hairline() -> Retained<NSColor> {
    NSColor::quaternaryLabelColor()
}

fn label() -> Retained<NSColor> {
    NSColor::labelColor()
}

fn secondary() -> Retained<NSColor> {
    NSColor::secondaryLabelColor()
}

fn tertiary() -> Retained<NSColor> {
    NSColor::tertiaryLabelColor()
}

fn text() -> Retained<NSFont> {
    NSFont::menuFontOfSize(TEXT_SIZE)
}

fn small() -> Retained<NSFont> {
    NSFont::menuFontOfSize(SMALL_SIZE)
}

fn semibold() -> Retained<NSFont> {
    // 0.3 is `NSFontWeightSemibold`.
    NSFont::systemFontOfSize_weight(TEXT_SIZE, 0.3)
}

/// Tabular figures, so `9%` and `11%` do not jitter as they change.
fn digits() -> Retained<NSFont> {
    NSFont::monospacedDigitSystemFontOfSize_weight(SMALL_SIZE, 0.0)
}

/// The baseline that centres `size`-point text in `bounds`.
fn mid_y(bounds: NSRect, size: f64) -> f64 {
    ((bounds.size.height - size * 1.25) / 2.0).floor()
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

fn draw_text(text: &str, x: f64, y: f64, font: &NSFont, color: &NSColor) {
    let string = NSString::from_str(text);
    // SAFETY: inside `drawRect:`, so there is a current graphics context.
    unsafe {
        string.drawAtPoint_withAttributes(NSPoint::new(x, y), Some(&attributes(font, color)));
    }
}

/// Truncated with an ellipsis at `width`: nothing may overrun its column.
fn draw_clipped(text: &str, x: f64, y: f64, width: f64, font: &NSFont, color: &NSColor) {
    if text_size(text, font).width <= width {
        draw_text(text, x, y, font, color);
        return;
    }
    let mut fitted: String = text.to_owned();
    while !fitted.is_empty() {
        fitted.pop();
        let candidate = format!("{fitted}…");
        if text_size(&candidate, font).width <= width {
            draw_text(&candidate, x, y, font, color);
            return;
        }
    }
}

fn text_size(text: &str, font: &NSFont) -> NSSize {
    let string = NSString::from_str(text);
    let color = NSColor::labelColor();
    // SAFETY: measurement only; no graphics context required.
    unsafe { string.sizeWithAttributes(Some(&attributes(font, &color))) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_percentage_draws_between_empty_and_full_and_small_ones_are_visible() {
        assert_eq!(fill_width(0.0, 64.0), 0.0, "zero is empty, and only zero");
        assert!(fill_width(1.0, 64.0) >= MIN_FILL);
        assert!(fill_width(12.0, 64.0) > fill_width(2.0, 64.0) + 4.0);
        assert_eq!(fill_width(100.0, 64.0), 64.0);
        // Providers have reported over 100 before; the bar stops at full.
        assert_eq!(fill_width(140.0, 64.0), 64.0);
        assert_eq!(fill_width(-5.0, 64.0), 0.0);
    }
}
