//! A label is not an opening decision. The destination is shown first, and
//! only an explicit second action may open an HTTP(S) URL, never a file.
use super::{
    ansi::{self, ACCENT, MUTED, RESET, TEXT},
    draw::{Hit, Sink},
    App, Awaiting, Panel,
};
use crate::driver::Driver;
use runeengine::Command;
use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub row: u16,
    pub columns: Range<usize>,
    pub url: String,
}

/// OSC-8 regions from the finished frame, after wrapping, clipping and panes.
pub fn regions(line: &str, row: u16) -> Vec<Link> {
    let mut links = Vec::new();
    let mut active: Option<(usize, String)> = None;
    let mut visible = String::new();
    let mut at = 0;
    while at < line.len() {
        let rest = &line[at..];
        if let Some(payload) = rest.strip_prefix("\x1b]8;") {
            let end = payload
                .find("\x1b\\")
                .map(|end| (end, 2))
                .or_else(|| payload.find('\x07').map(|end| (end, 1)));
            let Some((end, terminator)) = end else { break };
            if let Some((_, url)) = payload[..end].split_once(';') {
                let column = ansi::width(&visible);
                if let Some((start, url)) = active.take() {
                    if column > start {
                        links.push(Link {
                            row,
                            columns: start..column,
                            url,
                        });
                    }
                }
                if !url.is_empty() {
                    active = Some((column, url.into()));
                }
            }
            at += 4 + end + terminator;
        } else if rest.starts_with("\x1b[") {
            let bytes = rest.as_bytes();
            let mut end = 2;
            while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                end += 1;
            }
            at += (end + 1).min(bytes.len());
        } else if let Some(payload) = rest.strip_prefix("\x1b]") {
            let Some((end, size)) = payload
                .find("\x1b\\")
                .map(|end| (end, 2))
                .or_else(|| payload.find('\x07').map(|end| (end, 1)))
            else {
                break;
            };
            at += 2 + end + size;
        } else {
            let ch = rest.chars().next().expect("nonempty suffix");
            visible.push(ch);
            at += ch.len_utf8();
        }
    }
    links
}

/// Keep native terminal links only for validated web URLs. Path links remain
/// in Rune's frame hit map, but can never launch a file association by a native
/// terminal modifier-click.
pub fn web_only(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut at = 0;
    let mut web = false;
    while let Some(next) = line[at..].find("\x1b]8;") {
        let start = at + next;
        out.push_str(&line[at..start]);
        let payload = &line[start + 4..];
        let Some(end) = payload.find("\x1b\\") else {
            return out;
        };
        let stop = start + 4 + end + 2;
        let url = payload[..end].split_once(';').map_or("", |(_, url)| url);
        if url.is_empty() {
            if web {
                out.push_str(&line[start..stop]);
            }
            web = false;
        } else {
            web = runeengine::platform::BrowserUrl::parse(url).is_ok();
            if web {
                out.push_str(&line[start..stop]);
            }
        }
        at = stop;
    }
    out.push_str(&line[at..]);
    out
}

pub fn show(app: &mut App, url: String) {
    app.show_panel(Panel::Link);
    app.link_target = Some(url);
}

pub fn render(app: &App, width: usize) -> Sink {
    let mut out = Sink::default();
    out.append(&super::panels::title(
        "REFERENCE",
        "Review the complete destination before opening it",
        width,
    ));
    let url = app.link_target.as_deref().unwrap_or("");
    for line in ansi::wrap(&ansi::tame(url), width.saturating_sub(4).max(1)) {
        out.content("  ", &format!("{TEXT}{line}{RESET}"), "", Hit::None);
    }
    out.push(String::new());
    if runeengine::platform::BrowserUrl::parse(url).is_ok() {
        out.content(
            "  ",
            &format!("{ACCENT}Open in browser{RESET}"),
            "",
            Hit::LinkOpen,
        );
        out.push(format!(
            "  {MUTED}Enter opens this URL using your system browser.{RESET}"
        ));
    } else {
        out.push(format!("  {MUTED}Path or unsupported link: copy only. Rune does not open or execute files.{RESET}"));
    }
    out.content(
        "  ",
        &format!("{ACCENT}Copy complete destination{RESET}"),
        "",
        Hit::LinkCopy,
    );
    out.push(format!(
        "  {MUTED}C copy · Esc close · drag to select{RESET}"
    ));
    out
}

pub fn open(app: &mut App, driver: &mut Driver) {
    let Some(url) = app.link_target.clone() else {
        return;
    };
    if let Err(error) = runeengine::platform::BrowserUrl::parse(&url) {
        app.note(error.to_string());
        return;
    }
    app.request(driver, Awaiting::LinkOpened, |id| Command::OpenUrl {
        id,
        url,
    });
}

pub fn copy(app: &mut App) {
    if let Some(url) = &app.link_target {
        super::term::copy(url);
        app.note("Complete destination copied.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clicking_a_label_reviews_the_complete_destination_but_dragging_only_copies() {
        use super::super::{
            blocks::Block,
            draw,
            input::{Click, Key, Modifiers, Mouse},
            term,
        };
        let mut driver = super::super::test_driver("link-review");
        let mut app = App::for_test();
        app.blocks.push(Block::Text {
            text: "Read [official docs](https://example.com/a?x=1&y=2) or [source](src/main.rs)."
                .into(),
        });
        draw::compose_for_test(&mut app, 28, 95);
        let link = app
            .links
            .iter()
            .find(|link| link.url.starts_with("https:"))
            .unwrap()
            .clone();
        let click = |app: &mut App, driver: &mut Driver, kind, col| {
            app.on_key(
                Key::Mouse(Mouse {
                    kind,
                    row: link.row,
                    col,
                    modifiers: Modifiers::NONE,
                }),
                driver,
            )
        };
        click(
            &mut app,
            &mut driver,
            Click::Press,
            link.columns.start as u16 + 1,
        );
        click(
            &mut app,
            &mut driver,
            Click::Release,
            link.columns.start as u16 + 1,
        );
        assert_eq!(app.panel, Panel::Link);
        assert_eq!(
            app.link_target.as_deref(),
            Some("https://example.com/a?x=1&y=2")
        );
        let panel = draw::compose_for_test(&mut app, 28, 95)
            .iter()
            .map(|row| ansi::plain(row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(panel.contains("https://example.com/a?x=1&y=2"));
        assert!(panel.contains("Open in browser"));
        assert!(!app
            .awaiting
            .values()
            .any(|waiting| matches!(waiting, Awaiting::LinkOpened)));
        app.show_panel(Panel::None);
        draw::compose_for_test(&mut app, 28, 95);
        term::take_copies();
        click(
            &mut app,
            &mut driver,
            Click::Press,
            link.columns.start as u16 + 1,
        );
        click(&mut app, &mut driver, Click::Drag, link.columns.end as u16);
        click(
            &mut app,
            &mut driver,
            Click::Release,
            link.columns.end as u16,
        );
        assert_eq!(app.panel, Panel::None);
        assert!(!term::take_copies().is_empty());
        show(&mut app, "src/main.rs".into());
        let file = render(&app, 90).lines.join("\n");
        assert!(file.contains("src/main.rs"));
        assert!(!file.contains("Open in browser"));
        driver.engine.shutdown();
    }

    #[test]
    fn reference_document_opens_at_its_own_top() {
        let mut app = App::for_test();
        show(&mut app, "https://example.com/very-long-path".into());
        let document = super::super::panels::document(&app, 30);
        assert!(document.from_top);
        assert_eq!(app.panel_view.top(Panel::Link, &document, 30, 5), 0);
    }

    #[test]
    fn frame_regions_follow_real_columns_and_closed_osc8_ranges() {
        let line = format!("xx {} end", ansi::link("https://example.com/a", "文 docs"));
        assert_eq!(
            regions(&line, 3),
            vec![Link {
                row: 3,
                columns: 3..10,
                url: "https://example.com/a".into()
            }]
        );
        assert!(regions("plain", 1).is_empty());
        let file = ansi::link("file:///tmp/script.sh", "script");
        assert_eq!(web_only(&file), "script");
        assert_eq!(regions(&file, 1).len(), 1);
        assert_eq!(web_only(&line), line);
    }
}
