//! Copy follows renderer-owned content ranges, never guessed box glyphs. The
//! same clipped ranges paint a drag and extract it, so adjacent panes cannot leak.
use std::ops::Range;

use super::ansi;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Page,
    Sidebar,
    Footer,
    Input,
    Card(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyPiece {
    /// Relative to the span, so pane/layout shifts do not change source mapping.
    pub columns: Range<usize>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub columns: Range<usize>,
    pub region: Region,
    pub source: Option<Vec<CopyPiece>>,
}

impl Span {
    pub fn new(columns: Range<usize>) -> Self {
        Self {
            columns,
            region: Region::Page,
            source: None,
        }
    }

    pub fn selected_text(&self, row: &str, columns: Range<usize>) -> String {
        match &self.source {
            Some(pieces) => pieces
                .iter()
                .filter(|piece| {
                    let start = self.columns.start + piece.columns.start;
                    let end = self.columns.start + piece.columns.end;
                    start < columns.end && end > columns.start
                })
                .map(|piece| piece.text.as_str())
                .collect(),
            None => ansi::slice(row, columns.start, columns.end)
                .trim_end()
                .to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub text: String,
    pub spans: Vec<Span>,
}

impl From<String> for Row {
    fn from(text: String) -> Self {
        let spans = vec![Span::new(0..ansi::width(&text))];
        Self { text, spans }
    }
}

impl From<&str> for Row {
    fn from(text: &str) -> Self {
        Self::from(text.to_owned())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub row: u16,
    pub col: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Point,
    pub head: Point,
    pub moved: bool,
}

impl Selection {
    fn ends(self) -> (Point, Point) {
        if (self.anchor.row, self.anchor.col) <= (self.head.row, self.head.col) {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    fn region(self, rows: &[Row]) -> Option<Region> {
        let row = rows.get(usize::from(self.anchor.row.checked_sub(1)?))?;
        let col = usize::from(self.anchor.col.saturating_sub(1));
        row.spans
            .iter()
            .find(|span| span.columns.contains(&col))
            .map(|span| span.region)
    }

    pub fn columns(self, rows: &[Row], row: usize) -> Vec<Range<usize>> {
        let Some(region) = self.region(rows) else {
            return Vec::new();
        };
        let (start, end) = self.ends();
        let at = row + 1;
        if at < usize::from(start.row) || at > usize::from(end.row) {
            return Vec::new();
        }
        let from = if at == usize::from(start.row) {
            usize::from(start.col.saturating_sub(1))
        } else {
            0
        };
        let to = if at == usize::from(end.row) {
            usize::from(end.col.saturating_sub(u16::from(region == Region::Input)))
        } else {
            usize::MAX
        };
        let (from, to) = if region == Region::Input {
            let Some(line) = rows.get(row) else {
                return Vec::new();
            };
            let boundary =
                |column| ansi::width(&line.text[..ansi::byte_at_column(&line.text, column)]);
            (boundary(from), boundary(to))
        } else {
            (from, to)
        };
        rows.get(row)
            .into_iter()
            .flat_map(|row| &row.spans)
            .filter(|span| span.region == region)
            .filter_map(|span| {
                let range = from.max(span.columns.start)..to.min(span.columns.end);
                (range.start < range.end).then_some(range)
            })
            .collect()
    }

    pub fn selected_text(self, rows: &[Row]) -> String {
        if !self.moved {
            return String::new();
        }
        let Some(region) = self.region(rows) else {
            return String::new();
        };
        let (start, end) = self.ends();
        let mut selected = Vec::new();
        for row in usize::from(start.row.saturating_sub(1))..usize::from(end.row).min(rows.len()) {
            let columns = self.columns(rows, row);
            if !columns.is_empty() {
                let mapped = rows[row].spans.iter().any(|span| {
                    span.region == region
                        && span.source.is_some()
                        && columns.iter().any(|range| {
                            span.columns.start < range.end && span.columns.end > range.start
                        })
                });
                let line = columns
                    .into_iter()
                    .map(|range| {
                        rows[row]
                            .spans
                            .iter()
                            .find(|span| {
                                span.region == region
                                    && span.columns.start <= range.start
                                    && span.columns.end >= range.end
                            })
                            .map(|span| span.selected_text(&rows[row].text, range))
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
                    .join("\t");
                selected.push(if mapped {
                    line
                } else {
                    line.trim_end().to_owned()
                });
            }
        }
        // Leading source indentation is content, not UI padding.
        while selected.first().is_some_and(String::is_empty) {
            selected.remove(0);
        }
        while selected.last().is_some_and(String::is_empty) {
            selected.pop();
        }
        selected.join("\n")
    }
}

/// The source range uses the editor's glyph-safe, UTF-8 boundary mapping. A
/// drag-copy is not a caret placement or edit transaction.
pub(super) fn source(text: &str, start: usize, end: usize) -> String {
    text.get(start.min(end)..start.max(end))
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiline_copy_and_highlight_stay_inside_the_origin_pane() {
        let rows = vec![
            Row {
                text: "alpha       │ side-one".into(),
                spans: vec![
                    Span::new(0..5),
                    Span {
                        columns: 14..22,
                        region: Region::Sidebar,
                        source: None,
                    },
                ],
            },
            Row {
                text: "beta        │ side-two".into(),
                spans: vec![
                    Span::new(0..4),
                    Span {
                        columns: 14..22,
                        region: Region::Sidebar,
                        source: None,
                    },
                ],
            },
        ];
        for (anchor, head, expected) in [
            (
                Point { row: 1, col: 1 },
                Point { row: 2, col: 4 },
                "alpha\nbeta",
            ),
            (
                Point { row: 2, col: 4 },
                Point { row: 1, col: 1 },
                "alpha\nbeta",
            ),
            (
                Point { row: 1, col: 15 },
                Point { row: 2, col: 22 },
                "side-one\nside-two",
            ),
        ] {
            let selection = Selection {
                anchor,
                head,
                moved: true,
            };
            assert_eq!(selection.selected_text(&rows), expected);
            assert!(selection
                .columns(&rows, 0)
                .iter()
                .all(|r| r.end <= 5 || r.start >= 14));
        }
    }

    #[test]
    fn projected_command_copies_source_newlines_and_ignores_display_only_marks() {
        let preview = super::super::terminal_preview::command(
            "sh",
            &["mkdir a\necho abc\necho hidden".into()],
            24,
        );
        let shown = ansi::plain(&preview.text);
        assert!(shown.contains('↵') && shown.ends_with('…'));
        for shift in [2, 10, 30] {
            let first = shift;
            let last = first + preview.copy.last().map_or(0, |piece| piece.columns.end);
            let rows = vec![Row {
                text: format!("{}{} │ neighboring pane", " ".repeat(shift), shown),
                spans: vec![Span {
                    columns: first..last,
                    region: Region::Page,
                    source: Some(preview.copy.clone()),
                }],
            }];
            let selected = Selection {
                anchor: Point {
                    row: 1,
                    col: first as u16 + 1,
                },
                head: Point {
                    row: 1,
                    col: last as u16 + 10,
                },
                moved: true,
            };
            let copied = selected.selected_text(&rows);
            assert!("mkdir a\necho abc\necho hidden".starts_with(&copied));
            assert!(copied.contains("mkdir a\necho abc"), "{copied:?}");
            assert!(!copied.contains('↵') && !copied.contains('…'));
            assert!(!copied.contains("neighboring"));
            let marker = shift + ansi::width(shown.split('↵').next().unwrap());
            assert_eq!(
                rows[0].spans[0].selected_text(&rows[0].text, marker..marker + 1),
                "\n"
            );
        }
    }

    #[test]
    fn projected_copy_preserves_trailing_source_newlines_and_literal_marker_glyphs() {
        for source in ["echo one\n", "echo one\n\n", "echo '↵ …'\necho next"] {
            let preview = super::super::terminal_preview::command("sh", &[source.into()], 72);
            let width = ansi::width(&preview.text);
            let rows = vec![Row {
                text: ansi::plain(&preview.text),
                spans: vec![Span {
                    columns: 0..width,
                    region: Region::Page,
                    source: Some(preview.copy),
                }],
            }];
            let selection = Selection {
                anchor: Point { row: 1, col: 1 },
                head: Point {
                    row: 1,
                    col: width as u16,
                },
                moved: true,
            };
            assert_eq!(selection.selected_text(&rows), source);
        }
    }

    #[test]
    fn furniture_is_skipped_but_literal_box_glyphs_and_indentation_survive() {
        let rows = vec![
            Row {
                text: "│   code │ data  │".into(),
                spans: vec![Span::new(2..15)],
            },
            Row {
                text: "├───────────────┤".into(),
                spans: vec![],
            },
            Row {
                text: "│   next        │".into(),
                spans: vec![Span::new(2..15)],
            },
        ];
        let selection = Selection {
            anchor: Point { row: 1, col: 3 },
            head: Point { row: 3, col: 15 },
            moved: true,
        };
        assert_eq!(selection.selected_text(&rows), "  code │ data\n  next");
        let border = Selection {
            anchor: Point { row: 2, col: 3 },
            ..selection
        };
        assert!(border.selected_text(&rows).is_empty());
    }
}
