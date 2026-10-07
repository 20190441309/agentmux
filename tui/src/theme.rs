//! OpenCode-inspired neutral surfaces with a warm primary accent.
//! Color is semantic; whitespace separates conversation and controls.
//!
//! AGENTMUX_THEME selects dark (default), light or terminal surfaces.
//! Renderers never contain raw palette colors.

use agentmux_core::SessionState;
use ratatui::style::{Color, Modifier, Style};

// --- hues ---------------------------------------------------------------

/// Warm peach — agent identity, primary actions and input focus.
const ACCENT: Color = Color::Rgb(0xfa, 0xb2, 0x83);
/// Sage — done states, `+` diff lines, success.
const SUCCESS: Color = Color::Rgb(0x7f, 0xd8, 0x8f);
/// Sand — in-flight work, waiting, warnings.
const WARNING: Color = Color::Rgb(0xe5, 0xc0, 0x7b);
/// Terracotta — errors, `-` diff lines.
const ERROR: Color = Color::Rgb(0xc9, 0x82, 0x7d);
/// Mauve — permission notices, relay picks.
const SPECIAL: Color = Color::Rgb(0x9d, 0x7c, 0xd8);
/// Light gray — primary readable text.
const TEXT: Color = Color::Rgb(0xee, 0xee, 0xee);
/// Medium gray — secondary text (labels, hints).
const DIM: Color = Color::Rgb(0x99, 0x99, 0x99);
/// Dark gray — tertiary chrome (timestamps, borders, separators).
const FAINT: Color = Color::Rgb(0x70, 0x70, 0x70);
/// Near-black — the overlay backdrop wash.
const SHADE: Color = Color::Rgb(0x0a, 0x0a, 0x0a);

/// Named styles for every role the UI renders. Look up a field rather
/// than styling ad-hoc — the palette stays coherent by construction.
pub struct Theme {
    /// Primary readable text.
    pub text: Style,
    pub panel: Style,
    pub input: Style,
    pub control: Style,
    pub primary: Style,
    pub code: Style,
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
            panel: Style::new().fg(TEXT).bg(Color::Rgb(0x14, 0x14, 0x14)),
            input: Style::new().fg(TEXT).bg(Color::Rgb(0x1e, 0x1e, 0x1e)),
            control: Style::new().fg(DIM),
            primary: Style::new()
                .fg(SHADE)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
            code: Style::new().fg(TEXT).bg(Color::Rgb(0x1e, 0x1e, 0x1e)),
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
            selection: Style::new().fg(TEXT).bg(Color::Rgb(0x28, 0x28, 0x28)),
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
}

/// The selected palette, initialized once before the first frame.
pub static THEME: std::sync::LazyLock<Theme> = std::sync::LazyLock::new(|| {
    let mut theme = Theme::new();
    let mode = std::env::var("AGENTMUX_THEME").unwrap_or_else(|_| "dark".into());
    if mode == "light" {
        let text = Color::Rgb(0x24, 0x2a, 0x31);
        let dim = Color::Rgb(0x62, 0x6b, 0x72);
        let blue = Color::Rgb(0x34, 0x67, 0x91);
        theme.text = Style::new().fg(text);
        theme.dim = Style::new().fg(dim);
        theme.faint = Style::new().fg(dim);
        theme.accent = Style::new().fg(blue);
        theme.success = Style::new().fg(Color::Rgb(0x3d, 0x71, 0x50));
        theme.warning = Style::new().fg(Color::Rgb(0x87, 0x65, 0x1f));
        theme.error = Style::new().fg(Color::Rgb(0xa5, 0x48, 0x40));
        theme.special = Style::new().fg(Color::Rgb(0x79, 0x54, 0x8e));
        theme.panel = theme.text.bg(Color::Rgb(0xf0, 0xef, 0xeb));
        theme.input = theme.text.bg(Color::Rgb(0xee, 0xed, 0xe8));
        theme.control = theme.dim;
        theme.primary = Style::new()
            .fg(Color::White)
            .bg(blue)
            .add_modifier(Modifier::BOLD);
        theme.code = theme.input;
        theme.selection = theme.text.bg(Color::Rgb(0xe0, 0xe7, 0xeb));
        theme.border = theme.dim;
        theme.border_focus = theme.accent;
        theme.title = theme.dim;
        theme.section = theme.dim.add_modifier(Modifier::BOLD);
        theme.accent_bold = theme.accent.add_modifier(Modifier::BOLD);
        theme.warning_bold = theme.warning.add_modifier(Modifier::BOLD);
        theme.special_bold = theme.special.add_modifier(Modifier::BOLD);
        theme.dim_italic = theme.dim.add_modifier(Modifier::ITALIC);
        theme.faint_italic = theme.dim_italic;
        theme.backdrop = Style::new().bg(Color::Rgb(0xfa, 0xf9, 0xf6));
    } else if mode == "terminal" {
        theme.text = Style::new();
        theme.panel = Style::new();
        theme.input = Style::new();
        theme.selection = Style::new().add_modifier(Modifier::REVERSED);
        theme.backdrop = Style::new();
        theme.control = Style::new();
        theme.primary = theme.selection.add_modifier(Modifier::BOLD);
        theme.code = Style::new().add_modifier(Modifier::BOLD);
        theme.dim = Style::new().add_modifier(Modifier::DIM);
        theme.title = theme.dim;
        theme.faint = theme.dim;
    }
    theme
});

/// Syntax palette adapted from OpenCode (see design/OPENCODE-NOTICE.md).
pub static CODE_THEME: std::sync::LazyLock<tui_markdown::CodeTheme> = std::sync::LazyLock::new(
    || {
        let color = |style: Style| match style.fg {
            Some(Color::Rgb(r, g, b)) => format!("#{r:02x}{g:02x}{b:02x}"),
            _ => "#eeeeee".into(),
        };
        let scopes = [
            ("", THEME.text),
            ("comment", THEME.dim),
            ("keyword, storage", THEME.special),
            ("string", THEME.success),
            ("constant.numeric, constant.language", THEME.warning),
            ("entity.name.function, support.function", THEME.accent),
            ("entity.name.type, support.type", THEME.warning),
            ("variable", THEME.error),
        ];
        let settings: String = scopes.into_iter().map(|(scope, style)| format!(
        "<dict><key>scope</key><string>{scope}</string><key>settings</key><dict><key>foreground</key><string>{}</string></dict></dict>", color(style))).collect();
        tui_markdown::CodeTheme::from_textmate(&format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>name</key><string>OpenCode</string><key>settings</key><array>{settings}</array></dict></plist>"))
        .expect("bundled OpenCode syntax palette")
    },
);

#[cfg(test)]
mod tests {
    use super::*;

    /// The theme exists and gives each semantic role a distinct
    /// foreground (palettes that collapse to monochrome fail here).
    #[test]
    fn semantic_roles_have_distinct_colors() {
        let theme = Theme::new();
        assert_ne!(theme.accent.fg, theme.error.fg);
        assert_ne!(theme.success.fg, theme.warning.fg);
        assert_ne!(theme.dim.fg, theme.faint.fg);
        assert_eq!(
            theme.badge(&SessionState::Done).fg,
            theme.success.fg,
            "done maps to the success hue"
        );
        assert_eq!(
            theme.badge(&SessionState::Error("x".into())).fg,
            theme.error.fg
        );
    }
}
