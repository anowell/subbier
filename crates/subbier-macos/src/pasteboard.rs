//! The general pasteboard, for "Copy env snippet".

use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

/// Returns whether the write was accepted.
pub fn copy(text: &str) -> bool {
    let pasteboard = NSPasteboard::generalPasteboard();
    // Without `clearContents` the new item joins whatever the previous owner declared.
    let _change_count = pasteboard.clearContents();
    let value = NSString::from_str(text);
    // SAFETY: reading an `extern` static objc2 declares correctly.
    let ty = unsafe { NSPasteboardTypeString };
    pasteboard.setString_forType(&value, ty)
}
