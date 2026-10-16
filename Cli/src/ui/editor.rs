//! Rendering and clicking share one plain projection: a tab or an escape must
//! never make a mouse column address different source bytes than the caret does.
use super::ansi;

pub(super) struct Projection {
    pub text: String,
    source: Vec<usize>,
}

impl Projection {
    pub fn plain(input: &str) -> Self {
        let (text, source) = ansi::editor_text(input);
        Self { text, source }
    }

    pub fn single_line(input: &str) -> Self {
        let mut projection = Self::plain(input);
        projection.text = projection.text.replace('\n', " ");
        projection
    }

    pub fn masked(input: &str) -> Self {
        let mut text = String::new();
        let mut source = vec![0];
        for (at, glyph, _) in ansi::editor_glyphs(input) {
            text.push('•');
            source.extend([at, at, at + glyph.len()]);
        }
        Self { text, source }
    }

    pub fn display_at(&self, cursor: usize) -> usize {
        self.source
            .partition_point(|&offset| offset < cursor)
            .min(self.text.len())
    }

    pub fn source_at(&self, at: usize) -> usize {
        self.source[at.min(self.text.len())]
    }

    pub fn row(&self, start: usize, text: &str, first_col: usize) -> Row {
        Row {
            start: self.source[start],
            text: text.to_owned(),
            source: self.source[start..=start + text.len()]
                .iter()
                .map(|offset| offset - self.source[start])
                .collect(),
            first_col,
        }
    }

    /// A single-line field scrolls by whole glyphs, not half a wide cell. Keeping
    /// one blank cell after the text makes its end caret visible at any width.
    pub fn window(&self, cursor: usize, width: usize, first_col: usize) -> (Row, usize) {
        let at = self.display_at(cursor);
        let before = ansi::width(&self.text[..at]);
        let width = width.max(1);
        let from_col = before.saturating_sub(width.saturating_sub(1));
        let from = ansi::byte_at_column(&self.text, from_col);
        let mut end = from;
        let mut used = 0;
        for (offset, glyph, columns) in ansi::editor_glyphs(&self.text[from..]) {
            if used + columns > width {
                break;
            }
            end = from + offset + glyph.len();
            used += columns;
        }
        let here = first_col + ansi::width(&self.text[from..at.max(from)]);
        (self.row(from, &self.text[from..end], first_col), here)
    }
}

/// A renderer-published row; source is relative to start, as in the original
/// ComposerRow. It carries no field identity: that belongs to the owning form.
pub(super) struct Row {
    pub start: usize,
    pub text: String,
    pub source: Vec<usize>,
    pub first_col: usize,
}

impl Row {
    pub fn end(&self) -> usize {
        self.start + self.source.last().copied().unwrap_or(0)
    }

    pub fn styled(&self, selected: Option<std::ops::Range<usize>>) -> String {
        let Some(selected) = selected else {
            return self.text.clone();
        };
        let mut out = String::new();
        let mut active = false;
        for (offset, glyph, _) in ansi::editor_glyphs(&self.text) {
            let from = self.start + self.source[offset];
            let to = self.start + self.source[offset + glyph.len()];
            let marked = from < selected.end && to > selected.start;
            if marked != active {
                out.push_str(if marked { "\x1b[7m" } else { "\x1b[27m" });
                active = marked;
            }
            out.push_str(glyph);
        }
        if active {
            out.push_str("\x1b[27m");
        }
        out
    }

    pub fn caret_at(&self, col: u16) -> usize {
        let column = (col as usize).saturating_sub(self.first_col);
        self.start + self.source[ansi::byte_at_column(&self.text, column)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_surface_maps_plain_wrapped_and_scrolled_glyphs_to_source_bytes() {
        for input in [
            "",
            "hello",
            "a\té界",
            "e\u{301}👨\u{200d}💻end",
            "a\x1b[31mé\x1b[0m\x07z",
        ] {
            let projection = Projection::plain(input);
            for width in 1..=20 {
                for (start, text) in super::super::draw::compose_rows(&projection.text, width) {
                    let row = projection.row(start, &text, 9);
                    for col in 0..=width + 10 {
                        let at = row.caret_at(col as u16);
                        assert!(input.is_char_boundary(at), "{input:?} at {col}");
                        assert!(at <= input.len());
                    }
                    assert_eq!(
                        row.caret_at(u16::MAX),
                        projection.source[start + text.len()]
                    );
                }
                for at in (0..=input.len()).filter(|&at| input.is_char_boundary(at)) {
                    let (row, col) = projection.window(at, width, 7);
                    assert!(
                        col >= 7 && col < 7 + width,
                        "{input:?} width {width}, caret {at}, col {col}"
                    );
                    assert!(ansi::width(&row.text) <= width);
                    assert!(input.is_char_boundary(row.caret_at(col as u16)));
                }
            }
        }
    }

    #[test]
    fn a_masked_unicode_field_maps_bullets_without_exposing_the_secret() {
        let input = "é界🔑";
        let projection = Projection::masked(input);
        assert_eq!(projection.text, "•••");
        let (row, col) = projection.window(input.len(), 10, 7);
        assert_eq!(col, 10);
        assert_eq!(row.caret_at(8), "é".len());
        assert_eq!(row.caret_at(9), "é界".len());
        assert_eq!(row.caret_at(100), input.len());
        let input = "e\u{301}👩\u{200d}💻x";
        let projection = Projection::masked(input);
        assert_eq!(projection.text, "•••");
        let (row, _) = projection.window(input.len(), 10, 7);
        assert_eq!(row.caret_at(8), "e\u{301}".len());
        assert_eq!(row.caret_at(9), "e\u{301}👩\u{200d}💻".len());
    }
}
