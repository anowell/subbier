//! Terminal colour for [`crate::report`]'s rows. Hues are the *basic* ANSI
//! colours, not sRGB triples, so they suit whatever palette the user picked.
//! Every method closes with a full reset, so painting an already-painted string
//! ends the outer style early: compose plain strings, then paint once.

use std::io::IsTerminal as _;

use libsubby::Severity;

use crate::report::{Paint, Row};

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";

#[derive(Debug, Clone, Copy)]
pub(crate) struct Style {
    color: bool,
}

impl Style {
    /// `NO_COLOR` (any value, per <https://no-color.org>), `TERM=dumb` and a
    /// pipe each turn colour off.
    pub(crate) fn auto() -> Self {
        let wanted = std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true);
        Self {
            color: wanted && std::io::stdout().is_terminal(),
        }
    }

    #[cfg(test)]
    pub(crate) const fn plain() -> Self {
        Self { color: false }
    }

    fn paint(self, code: &str, text: &str) -> String {
        // A 0% bar has no filled cells, and codes around nothing are noise.
        if !self.color || text.is_empty() {
            return text.to_owned();
        }
        format!("{code}{text}{RESET}")
    }

    fn bold(self, text: &str) -> String {
        self.paint(BOLD, text)
    }

    fn dim(self, text: &str) -> String {
        self.paint(DIM, text)
    }

    fn red(self, text: &str) -> String {
        self.paint(RED, text)
    }

    pub(crate) fn line(self, row: &Row) -> String {
        row.spans
            .iter()
            .map(|span| self.run(span.paint, &span.text))
            .collect()
    }

    fn run(self, paint: Paint, text: &str) -> String {
        match paint {
            Paint::Plain => text.to_owned(),
            Paint::Dim => self.dim(text),
            Paint::Bold => self.bold(text),
            Paint::Alert => self.red(text),
            Paint::Sev(severity) => self.severity(severity, text),
        }
    }

    fn severity(self, severity: Severity, text: &str) -> String {
        self.paint(
            match severity {
                Severity::Ok => GREEN,
                Severity::Warn => YELLOW,
                Severity::Critical => RED,
            },
            text,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLOR: Style = Style { color: true };

    #[test]
    fn no_escapes_without_colour_or_content() {
        let plain = Style::plain();
        assert_eq!(plain.dim("x"), "x");
        assert_eq!(plain.bold("x"), "x");
        assert_eq!(plain.severity(Severity::Critical, "x"), "x");
        assert_eq!(COLOR.dim(""), "");
        assert_eq!(COLOR.severity(Severity::Ok, ""), "");
    }

    #[test]
    fn painted_text_always_closes_its_run() {
        for painted in [
            COLOR.dim("x"),
            COLOR.bold("x"),
            COLOR.red("x"),
            COLOR.severity(Severity::Warn, "x"),
        ] {
            assert!(painted.starts_with('\x1b'), "{painted:?}");
            assert!(painted.ends_with(RESET), "{painted:?}");
        }
    }
}
