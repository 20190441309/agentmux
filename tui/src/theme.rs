//! The UI's single palette — muted, low-saturation accents on the
//! terminal's default background (克制现代 / restrained modern: color is
//! semantic, whitespace does the grouping, nothing neon).
//!
//! All styles are `const`-built, so the palette is a plain [`THEME`]
//! constant — no lazy init, and `ui.rs` holds no scattered `Color::*`
//! literals. Terminals without truecolor degrade each accent to its
//! nearest ANSI shade, which stays low-key by construction.

use agentmux_core::SessionState;
use ratatui::style::{Color, Modifier, Style};

// --- hues ---------------------------------------------------------------

/// Dusty blue — agent identity, info, focus.
const ACCENT: Color = Color::Rgb(0x7e, 0xa2, 0xc8);
/// Sage — done states, `+` diff lines, success.
const SUCCESS: Color = Color::Rgb(0x8f, 0xb3, 0x8f);
/// Sand — in-flight work, waiting, warnings.
const WARNING: Color = Color::Rgb(0xc9, 0xb1, 0x7d);
/// Terracotta — errors, `-` diff lines.
const ERROR: Color = Color::Rgb(0xc9, 0x82, 0x7d);
/// Mauve — permission notices, relay picks.
const SPECIAL: Color = Color::Rgb(0xa8, 0x8f, 0xb8);
/// Light gray — primary readable text.
const TEXT: Color = Color::Rgb(0xc8, 0xc8, 0xc4);
/// Medium gray — secondary text (labels, hints).
const DIM: Color = Color::Rgb(0x77, 0x77, 0x73);
/// Dark gray — tertiary chrome (timestamps, borders, separators).
const FAINT: Color = Color::Rgb(0x50, 0x50, 0x4d);
/// Near-black — the overlay backdrop wash.
const SHADE: Color = Color::Rgb(0x12, 0x12, 0x16);

/// Named styles for every role the UI renders. Look up a field rather
/// than styling ad-hoc — the palette stays coherent by construction.
pub struct Theme {
    /// Primary readable text.
    pub text: Style,
    /// Secondary text: state labels, paths, hints.
    pub dim: Style,
    /// Tertiary chrome: timestamps, ids, separators.
    pub faint: Style,
    /// Info / agent-identity accent.
    pub accent: Style,
    /// Success accent: `Done` sessions, added diff lines.
    pub success: Style,
    /// Warning accent: in-flight work, permission banners.
    pub warning: Style,
    /// Error accent: failures, removed diff lines.
    pub error: Style,
    /// Special accent: permission/relay UI.
    pub special: Style,
    /// Workspace group headers — a quiet section label.
    pub section: Style,
    /// Agent-name message headers.
    pub accent_bold: Style,
    /// Permission banners.
    pub warning_bold: Style,
    /// `you` message headers.
    pub special_bold: Style,
    /// Thought prose, orchestrator notices.
    pub dim_italic: Style,
    /// "waiting…"-type quiet placeholders.
    pub faint_italic: Style,
    /// Pane border titles (kept dim — whitespace separates panes).
    pub title: Style,
    /// Pane borders at rest.
    pub border: Style,
    /// Border of the focused widget (input while `Editing`).
    pub border_focus: Style,
    /// Selected list row: a subtle reverse, no rainbow.
    pub selection: Style,
    /// Full-screen wash behind modal overlays.
    pub backdrop: Style,
}

impl Theme {
    const fn new() -> Theme {
        Theme {
            text: Style::new().fg(TEXT),
            dim: Style::new().fg(DIM),
            faint: Style::new().fg(FAINT),
            accent: Style::new().fg(ACCENT),
            success: Style::new().fg(SUCCESS),
            warning: Style::new().fg(WARNING),
            error: Style::new().fg(ERROR),
            special: Style::new().fg(SPECIAL),
            section: Style::new().fg(DIM).add_modifier(Modifier::BOLD),
            accent_bold: Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
            warning_bold: Style::new().fg(WARNING).add_modifier(Modifier::BOLD),
            special_bold: Style::new().fg(SPECIAL).add_modifier(Modifier::BOLD),
            dim_italic: Style::new().fg(DIM).add_modifier(Modifier::ITALIC),
            faint_italic: Style::new().fg(FAINT).add_modifier(Modifier::ITALIC),
            title: Style::new().fg(DIM),
            border: Style::new().fg(FAINT),
            border_focus: Style::new().fg(ACCENT),
            selection: Style::new().add_modifier(Modifier::REVERSED),
            backdrop: Style::new().bg(SHADE),
        }
    }

    /// Badge style for a [`SessionState`] — the semantic color of the
    /// session's current lifecycle step.
    pub fn badge(&self, state: &SessionState) -> Style {
        match state {
            SessionState::Created => self.dim,
            SessionState::Connecting => self.warning,
            SessionState::Ready => self.accent,
            SessionState::Prompting => self.warning,
            SessionState::WaitingPermission => self.special,
            SessionState::Done => self.success,
            SessionState::Error(_) => self.error,
        }
    }

    /// Style for an ACP `tool_call` status string (`pending`,
    /// `in_progress`, `completed`, `failed`); absent/other → dim.
    pub fn tool_status(&self, status: Option<&str>) -> Style {
        match status {
            Some("completed") => self.success,
            Some("failed") => self.error,
            Some("in_progress") | Some("pending") => self.warning,
            _ => self.dim,
        }
    }

    /// Foreground color for the status-bar mode chip.
    pub fn mode(&self, mode: crate::app::InputMode) -> Style {
        use crate::app::InputMode::*;
        match mode {
            Normal => self.accent,
            Editing => self.warning,
            RelayPick => self.special,
            NewSession => self.accent,
            Permission => self.warning,
        }
    }
}

/// The one palette instance — a `const`, so it's allocation-free and
/// usable inside `const` contexts.
pub const THEME: Theme = Theme::new();

#[cfg(test)]
mod tests {
    use super::*;

    /// The theme exists and gives each semantic role a distinct
    /// foreground (palettes that collapse to monochrome fail here).
    #[test]
    fn semantic_roles_have_distinct_colors() {
        assert_ne!(THEME.accent.fg, THEME.error.fg);
        assert_ne!(THEME.success.fg, THEME.warning.fg);
        assert_ne!(THEME.dim.fg, THEME.faint.fg);
        assert_eq!(
            THEME.badge(&SessionState::Done).fg,
            THEME.success.fg,
            "done maps to the success hue"
        );
        assert_eq!(
            THEME.badge(&SessionState::Error("x".into())).fg,
            THEME.error.fg
        );
    }
}
