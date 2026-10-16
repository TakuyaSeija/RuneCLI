//! Overlay readers keep the place the renderer actually drew, not a distance
//! from a document whose height changes when the side panel opens. Navigation
//! requests a reveal; only the next complete layout knows where that choice is.

use std::ops::Range;

use super::draw::Sink;
use super::Panel;

#[derive(Default)]
pub struct Document {
    pub sink: Sink,
    spans: Vec<Span>,
    pub focus: Option<Range<usize>>,
    pub from_top: bool,
    /// A revealed focus is set mid-screen, the rows scrolling under it, and
    /// stops when the document's end is drawn — the way Settings › Models
    /// moves. Otherwise a reveal scrolls only as far as the focus needs.
    pub centre: bool,
}

struct Span {
    key: String,
    rows: Range<usize>,
}

impl Document {
    pub fn reader() -> Self {
        Self {
            from_top: true,
            ..Self::default()
        }
    }

    pub fn append(&mut self, key: impl Into<String>, lines: Vec<String>) -> Range<usize> {
        let start = self.sink.lines.len();
        self.sink.extend(lines);
        let rows = start..self.sink.lines.len();
        if !rows.is_empty() {
            self.spans.push(Span {
                key: key.into(),
                rows: rows.clone(),
            });
        }
        rows
    }

    /// Like [`Document::append`], each row answering the pointer with
    /// `hit(row within the lines)`.
    pub fn append_at(
        &mut self,
        key: impl Into<String>,
        lines: Vec<String>,
        hit: impl Fn(usize) -> super::draw::Hit,
    ) -> Range<usize> {
        let start = self.sink.lines.len();
        for (k, line) in lines.into_iter().enumerate() {
            self.sink.push_at(line, hit(k));
        }
        let rows = start..self.sink.lines.len();
        if !rows.is_empty() {
            self.spans.push(Span {
                key: key.into(),
                rows: rows.clone(),
            });
        }
        rows
    }

    pub fn plain(sink: Sink) -> Self {
        let rows = 0..sink.lines.len();
        Self {
            sink,
            spans: vec![Span {
                key: "body".into(),
                rows,
            }],
            ..Self::default()
        }
    }

    fn anchor(&self, top: usize, width: usize) -> Option<Anchor> {
        self.spans
            .iter()
            .find(|span| span.rows.contains(&top))
            .map(|span| Anchor {
                key: span.key.clone(),
                row: top - span.rows.start,
                rows: span.rows.len(),
                width,
            })
    }

    fn anchored_top(&self, anchor: &Anchor, width: usize) -> Option<usize> {
        let span = self.spans.iter().find(|span| span.key == anchor.key)?;
        let row = if anchor.width == width {
            anchor.row
        } else {
            anchor.row.saturating_mul(span.rows.len()) / anchor.rows.max(1)
        };
        Some(span.rows.start + row.min(span.rows.len().saturating_sub(1)))
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Anchor {
    key: String,
    row: usize,
    rows: usize,
    width: usize,
}

/// The overlay's own reader state; it never borrows the transcript's anchor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PanelView {
    /// Rows above the document's end, retained for wheel/page compatibility.
    pub scroll: usize,
    pub drawn_rows: usize,
    pub drawn_viewport: usize,
    pub shown: Option<Panel>,
    anchor: Option<Anchor>,
    width: usize,
    reveal: bool,
    focus_visible: bool,
}

impl PanelView {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn reveal(&mut self) {
        self.reveal = true;
    }

    pub fn scroll_by(&mut self, up: bool, rows: usize) {
        let max = self.drawn_rows.saturating_sub(self.drawn_viewport);
        let next = if up {
            self.scroll.saturating_add(rows).min(max)
        } else {
            self.scroll.saturating_sub(rows)
        };
        if next != self.scroll {
            self.scroll = next;
            self.anchor = None;
        }
        self.reveal = false;
    }

    pub fn page(&self) -> usize {
        self.drawn_viewport.max(1) / 2 + 1
    }

    pub fn top(&mut self, panel: Panel, doc: &Document, width: usize, viewport: usize) -> usize {
        let viewport = viewport.max(1);
        let max = doc.sink.lines.len().saturating_sub(viewport);
        let opening = self.shown != Some(panel);
        let mut top = if opening {
            self.anchor = None;
            if doc.from_top {
                0
            } else {
                max
            }
        } else {
            self.anchor
                .as_ref()
                .and_then(|anchor| doc.anchored_top(anchor, width))
                .unwrap_or_else(|| max.saturating_sub(self.scroll))
                .min(max)
        };
        let reflow = self.width != width || self.drawn_viewport != viewport;
        if opening || self.reveal || (reflow && self.focus_visible) {
            if let (Some(focus), true) = (&doc.focus, doc.centre) {
                top = focus.start.saturating_sub(viewport / 2).min(max);
            } else if let Some(focus) = &doc.focus {
                let margin = usize::from(viewport >= 4);
                if focus.start < top + margin {
                    top = focus.start.saturating_sub(margin);
                } else if focus.end > top + viewport - margin {
                    top = if focus.len() + 2 * margin <= viewport {
                        focus.end.saturating_sub(viewport - margin)
                    } else {
                        focus.start.saturating_sub(margin)
                    };
                }
                top = top.min(max);
            }
        }
        self.focus_visible = doc
            .focus
            .as_ref()
            .is_some_and(|focus| focus.start >= top && focus.start < top + viewport);
        self.shown = Some(panel);
        self.width = width;
        self.drawn_rows = doc.sink.lines.len();
        self.drawn_viewport = viewport;
        self.scroll = max.saturating_sub(top);
        // Logs still follow their newest row when at the end. A deliberately
        // positioned reader keeps a semantic place through width changes.
        self.anchor = if top == max && !doc.from_top && !self.reveal {
            None
        } else {
            doc.anchor(top, width)
        };
        self.reveal = false;
        top
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(width: usize) -> Document {
        let mut doc = Document {
            from_top: true,
            ..Document::default()
        };
        for i in 0..30 {
            doc.append(
                format!("item:{i}"),
                vec![format!("item {i}"); if width < 80 { 4 } else { 2 }],
            );
        }
        doc
    }

    #[test]
    fn a_manual_reader_keeps_the_same_item_after_split_reflow() {
        let mut view = PanelView::default();
        let wide = document(100);
        assert_eq!(view.top(Panel::Context, &wide, 100, 10), 0);
        view.scroll_by(false, 16);
        assert_eq!(view.top(Panel::Context, &wide, 100, 10), 16);
        assert_eq!(view.top(Panel::Context, &document(60), 60, 10), 32);
        assert_eq!(view.top(Panel::Context, &wide, 100, 10), 16);
    }

    #[test]
    fn navigation_reveals_the_choice_but_pages_do_not_snap_back() {
        let mut view = PanelView::default();
        let mut doc = document(100);
        doc.focus = Some(40..42);
        view.top(Panel::Undo, &doc, 100, 10);
        view.scroll_by(true, 15);
        let reading = view.top(Panel::Undo, &doc, 100, 10);
        assert!(reading < 30);
        assert_eq!(view.top(Panel::Undo, &doc, 100, 10), reading);
        view.reveal();
        let top = view.top(Panel::Undo, &doc, 100, 10);
        assert!(top <= 40 && top + 10 >= 42);
        doc.focus = Some(2..4);
        view.reveal();
        assert!(view.top(Panel::Undo, &doc, 100, 10) <= 2);
    }

    /// Settings › Models' motion (owner, 2026-10-09): a revealed choice sits
    /// mid-screen, and the document stops scrolling once its end is drawn.
    #[test]
    fn a_centred_document_keeps_its_choice_mid_screen_until_the_end() {
        let mut view = PanelView::default();
        let mut doc = document(100);
        doc.centre = true;
        doc.focus = Some(30..32);
        view.reveal();
        assert_eq!(view.top(Panel::Undo, &doc, 100, 10), 25);
        doc.focus = Some(2..4);
        view.reveal();
        assert_eq!(view.top(Panel::Undo, &doc, 100, 10), 0);
        doc.focus = Some(58..60);
        view.reveal();
        assert_eq!(view.top(Panel::Undo, &doc, 100, 10), 50, "stops at the end");
    }
}
