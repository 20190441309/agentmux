//! OpenCode message surfaces and Markdown semantics adapted to ratatui.
//! Upstream references and MIT notice: design/OPENCODE-NOTICE.md.
use pulldown_cmark::{CodeBlockKind, Event, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use tui_markdown::{Options, StyleSheet};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::theme::THEME;

#[derive(Clone)]
struct Styles;
impl StyleSheet for Styles {
    fn heading(&self, _: u8) -> Style {
        THEME.special.add_modifier(Modifier::BOLD)
    }
    fn heading_marker(&self, _: u8) -> &str {
        ""
    }
    fn code(&self) -> Style {
        Style {
            fg: THEME.success.fg,
            ..Style::default()
        }
    }
    fn code_block_fence(&self) -> &str {
        ""
    }
    fn link(&self) -> Style {
        THEME.accent.add_modifier(Modifier::UNDERLINED)
    }
    fn blockquote(&self) -> Style {
        THEME.warning
    }
    fn list_marker(&self) -> Style {
        THEME.accent
    }
    fn table_header(&self) -> Style {
        THEME.accent_bold
    }
    fn table_border(&self) -> Style {
        THEME.border
    }
}

fn rendered(text: &str, width: usize, base: Style) -> Vec<Line<'static>> {
    let options = Options::new(Styles)
        .table_width(width.min(u16::MAX as usize) as u16)
        .code_theme(crate::theme::CODE_THEME.clone());
    tui_markdown::from_str_with_options(text, &options)
        .lines
        .into_iter()
        .map(|line| {
            let style = base.patch(line.style);
            Line::from(
                line.spans
                    .into_iter()
                    .map(|s| Span::styled(s.content.into_owned(), style.patch(s.style)))
                    .collect::<Vec<_>>(),
            )
            .style(style)
        })
        .collect()
}

pub fn prose(text: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let indent = width.saturating_sub(1).min(2);
    let body_width = width - indent;
    let mut out = Vec::new();
    let mut cursor = 0;
    let mut code: Option<(String, String)> = None;
    // Use CommonMark's parser for fenced, indented, nested and unfinished blocks.
    for (event, range) in Parser::new_ext(text, pulldown_cmark::Options::all()).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                out.extend(rendered(&text[cursor..range.start], body_width, style));
                let lang = match kind {
                    CodeBlockKind::Fenced(s) => {
                        s.split_whitespace().next().unwrap_or("").to_owned()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((lang, String::new()));
            }
            Event::Text(s) if code.is_some() => code.as_mut().unwrap().1.push_str(&s),
            Event::End(TagEnd::CodeBlock) => {
                if let Some((lang, source)) = code.take() {
                    out.extend(code_block(&source, &lang, body_width));
                }
                cursor = range.end;
            }
            _ => {}
        }
    }
    out.extend(rendered(&text[cursor..], body_width, style));
    let mut out = reflow(out, body_width);
    for line in &mut out {
        line.spans
            .insert(0, Span::styled(" ".repeat(indent), THEME.backdrop));
    }
    out
}

fn code_block(source: &str, lang: &str, width: usize) -> Vec<Line<'static>> {
    // Re-fence with a delimiter longer than any run in the source. This also
    // highlights incomplete streamed fences without eating literal backticks.
    let fence = "`".repeat(
        source
            .split(|c| c != '`')
            .map(str::len)
            .max()
            .unwrap_or(0)
            .max(2)
            + 1,
    );
    let input = format!("{fence}{lang}\n{source}\n{fence}");
    let mut lines = rendered(&input, width, THEME.code);
    // The synthetic newline only closes the fence; preserve actual blank code rows.
    if source.ends_with('\n') && lines.last().is_some_and(|l| l.width() == 0) {
        lines.pop();
    }
    if width < 6 {
        return lines
            .into_iter()
            .flat_map(|l| wrap_line(l, width))
            .collect();
    }
    let inside = width - 4;
    let label: String = if lang.is_empty() { "code" } else { lang }
        .graphemes(true)
        .scan(0, |n, g| {
            *n += g.width();
            (*n <= width - 4).then_some(g)
        })
        .collect();
    let heading = format!(
        "╭─ {label}{}╮",
        "─".repeat(width.saturating_sub(label.width() + 4))
    );
    let mut out = vec![Line::styled(heading, THEME.code.patch(THEME.border))];
    for line in lines {
        for row in wrap_line(line, inside) {
            let pad = inside.saturating_sub(row.width());
            let mut spans = vec![Span::styled("│ ", THEME.border)];
            spans.extend(row.spans.into_iter().map(|s| {
                Span::styled(
                    s.content,
                    THEME
                        .code
                        .patch(s.style)
                        .bg(THEME.code.bg.unwrap_or(ratatui::style::Color::Reset)),
                )
            }));
            spans.push(Span::styled(" ".repeat(pad + 1), THEME.code));
            spans.push(Span::styled("│", THEME.border));
            out.push(Line::from(spans).style(THEME.code));
        }
    }
    out.push(Line::styled(
        format!("╰{}╯", "─".repeat(width - 2)),
        THEME.code.patch(THEME.border),
    ));
    out
}

/// Wrap by display columns without cutting UTF-8, wide glyphs or indentation.
fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row = Line::default().style(line.style);
    let mut used = 0;
    for span in line.spans {
        for g in span.content.graphemes(true) {
            let expanded = if g == "\t" {
                " ".repeat(4 - used % 4)
            } else if g.chars().any(char::is_control) {
                String::new()
            } else {
                g.to_owned()
            };
            for g in expanded.graphemes(true) {
                let len = g.width();
                if used + len > width && used > 0 {
                    rows.push(row);
                    row = Line::default().style(line.style);
                    used = 0;
                }
                if let Some(last) = row.spans.last_mut().filter(|s| s.style == span.style) {
                    last.content.to_mut().push_str(g);
                } else {
                    row.spans.push(Span::styled(g.to_owned(), span.style));
                }
                used += len;
            }
        }
    }
    rows.push(row);
    rows
}

/// Cache physical rows for replies so drawing a long message only visits its
/// visible tail. Wrap prose at whitespace, preserving span styles and all text.
pub fn reflow(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for line in lines {
        if line.width() <= width {
            out.push(line);
            continue;
        }
        let mut row: Vec<Span<'static>> = Vec::new();
        let mut used = 0;
        let mut boundary = None;
        for span in line.spans {
            for g in span.content.graphemes(true) {
                let n = g.width();
                if used + n > width && !row.is_empty() {
                    let split = boundary.filter(|i| *i > 0).unwrap_or(row.len());
                    let tail = row.split_off(split);
                    out.push(Line::from(row).style(line.style));
                    row = tail;
                    used = row.iter().map(Span::width).sum();
                    boundary = None;
                }
                row.push(Span::styled(g.to_owned(), span.style));
                used += n;
                if g.chars().all(char::is_whitespace) {
                    boundary = Some(row.len());
                }
            }
        }
        out.push(Line::from(row).style(line.style));
    }
    // Coalesce neighboring styles; a long prose paragraph should not cache one
    // allocation per character after wrapping.
    for line in &mut out {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for s in std::mem::take(&mut line.spans) {
            if let Some(last) = spans.last_mut().filter(|last| last.style == s.style) {
                last.content.to_mut().push_str(&s.content);
            } else {
                spans.push(s);
            }
        }
        line.spans = spans;
    }
    out
}

pub fn user_panel(text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    if width < 5 {
        return text
            .lines()
            .flat_map(|s| wrap_line(Line::styled(s.to_owned(), THEME.panel), width))
            .collect();
    }
    let inside = width - 4;
    let mut content = vec![
        Line::styled("You", THEME.text.add_modifier(Modifier::BOLD)),
        Line::default(),
    ];
    content.extend(text.lines().map(|s| Line::styled(s.to_owned(), THEME.text)));
    content.push(Line::default());
    content
        .into_iter()
        .flat_map(|l| wrap_line(l, inside))
        .map(|row| {
            let padding = inside.saturating_sub(row.width());
            let mut spans = vec![Span::styled("▎", THEME.accent), Span::raw("  ")];
            spans.extend(row.spans);
            spans.push(Span::raw(" ".repeat(padding + 1)));
            Line::from(spans).style(THEME.panel)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn fenced_code_preserves_indentation_and_markers_with_highlight() {
        let lines = prose(
            "Before\n\n```rust\nfn main() {\n    println!(\"**你好**\");\n}\n```\n\nAfter",
            THEME.text,
            48,
        );
        let shown = text(&lines);
        assert!(shown.contains("╭─ rust"), "{shown}");
        assert!(shown.contains("    println!"));
        assert!(shown.contains("**你好**"));
        assert!(shown.contains("After"));
        assert!(lines.iter().all(|l| l.width() <= 48));
        let code = lines
            .iter()
            .find(|l| l.to_string().contains("fn main"))
            .unwrap();
        assert!(code.spans.iter().any(|s| s.style.fg == THEME.special.fg));
        assert!(lines
            .last()
            .unwrap()
            .spans
            .iter()
            .all(|s| s.style.bg != THEME.code.bg));
    }
    #[test]
    fn tilde_unknown_and_unfinished_code_wrap_without_losing_unicode() {
        for fence in ["```", "~~~"] {
            let source = "    你好世界🙂abc**literal**";
            let lines = prose(&format!("{fence}unknown\n{source}"), THEME.text, 18);
            let shown = text(&lines);
            assert!(shown.contains("╭─ unknown"), "{shown}");
            assert!(lines.iter().all(|l| l.width() <= 18), "{shown}");
            let recovered: String = lines
                .iter()
                .filter_map(|l| {
                    let s = l.to_string();
                    s.strip_prefix("  │ ")
                        .and_then(|s| s.strip_suffix('│'))
                        .map(|s| s.trim().to_owned())
                })
                .collect();
            assert_eq!(recovered, source.trim());
        }
    }
    #[test]
    fn user_panel_fills_every_row_and_keeps_literal_input() {
        let lines = user_panel("**literal**\n你好世界abcdefghijklmnopqrstuvwxyz", 24);
        assert!(lines.iter().all(|l| l.width() == 24));
        assert!(lines.iter().all(|l| l.style.bg == THEME.panel.bg));
        assert!(text(&lines).contains("**literal**"));
        assert_ne!(THEME.panel.bg, THEME.backdrop.bg);
    }
    #[test]
    fn nested_markdown_tables_and_tiny_width_do_not_panic() {
        let input = "## Title\n\n- **one**\n  - two `code`\n\n| A | B |\n|---|---|\n| 中文 | value |\n\n    indented code\n";
        for width in [1, 4, 16, 80] {
            let shown = text(&prose(input, THEME.text, width));
            let compact: String = shown.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(compact.contains("Title"));
            assert!(!compact.contains("**one**"));
        }
    }
}
