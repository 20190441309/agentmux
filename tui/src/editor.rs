//! One cell layout shared by rendering, mouse placement and vertical movement.
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub struct EditorLayout {
    pub lines: Vec<String>,
    stops: Vec<(usize, usize, usize)>,
}
impl EditorLayout {
    pub fn new(text: &str, width: u16) -> Self {
        let width = usize::from(width.max(1));
        let mut lines = vec![String::new()];
        let mut stops = Vec::new();
        let (mut row, mut col) = (0, 0);
        for (i, g) in text.grapheme_indices(true) {
            let display = if g == "\t" { "    " } else { g };
            let w = UnicodeWidthStr::width(display).min(width);
            if g != "\n" && col + w > width {
                lines.push(String::new());
                row += 1;
                col = 0;
            }
            // At a full-width hard newline the insertion point is the next
            // row's first cell, just as it is for a full-width end of input.
            if g == "\n" && col == width {
                stops.push((i, row + 1, 0));
            } else {
                stops.push((i, row, col));
            }
            if g == "\n" {
                lines.push(String::new());
                row += 1;
                col = 0;
            } else {
                lines[row].push_str(display);
                col += w;
            }
        }
        if col >= width {
            lines.push(String::new());
            row += 1;
            col = 0;
        }
        stops.push((text.len(), row, col));
        Self { lines, stops }
    }
    pub fn position(&self, cursor: usize) -> (usize, usize) {
        let (_, row, col) = self
            .stops
            .iter()
            .rev()
            .find(|(i, _, _)| *i <= cursor)
            .unwrap();
        (*row, *col)
    }
    pub fn cursor_at(&self, row: usize, col: usize) -> usize {
        self.stops
            .iter()
            .rev()
            .find(|(_, r, c)| *r < row || (*r == row && *c <= col))
            .map(|(i, _, _)| *i)
            .unwrap_or(0)
    }
}
