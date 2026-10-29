//! Real render and normal dispatcher contracts: clipboard bytes, not just a
//! status string, prove furniture stayed out and a drag did not become a click.
use super::{
    ansi,
    blocks::{Block, Tool, ToolState},
    draw::{self, Hit},
    input::{Click, Key, Modifiers, Mouse},
    selection::{Point, Selection},
    settings, term, App, Panel,
};
use crate::driver::Driver;

fn mouse(app: &mut App, driver: &mut Driver, kind: Click, point: Point) {
    app.on_key(
        Key::Mouse(Mouse {
            kind,
            row: point.row,
            col: point.col,
            modifiers: Modifiers::NONE,
        }),
        driver,
    );
}

fn drag(
    app: &mut App,
    driver: &mut Driver,
    first: Point,
    last: Point,
    rows: u16,
    cols: u16,
) -> Vec<String> {
    term::take_copies();
    mouse(app, driver, Click::Press, first);
    mouse(app, driver, Click::Drag, last);
    let highlighted = draw::compose_for_test(app, rows, cols);
    assert!(highlighted.iter().any(|line| line.contains(ansi::REVERSE)));
    mouse(app, driver, Click::Release, last);
    assert!(app.selection.is_none());
    let released = draw::compose_for_test(app, rows, cols);
    assert!(
        !released.iter().any(|line| line.contains(ansi::REVERSE)),
        "release left a highlight: {released:?}"
    );
    term::take_copies()
}

fn point(frame: &[String], needle: &str) -> Point {
    frame
        .iter()
        .enumerate()
        .find_map(|(row, line)| {
            let plain = ansi::plain(line);
            let at = plain.find(needle)?;
            Some(Point {
                row: row as u16 + 1,
                col: ansi::width(&plain[..at]) as u16 + 1,
            })
        })
        .unwrap_or_else(|| panic!("missing {needle:?}: {frame:?}"))
}

fn tool(name: &str, text: &str) -> Block {
    let mut tool = Tool::new("copy-tool".into(), name.into(), "copy fixture".into());
    if name == "terminal_use" {
        tool.shell = Some(runeengine::shell::ShellInfo::new(std::path::Path::new(
            "/bin/bash",
        )));
    }
    tool.state = ToolState::Ok;
    tool.content = text.into();
    tool.args =
        Some(serde_json::json!({"command":"printf fixture", "path":"fixture.rs", "text":text}));
    Block::of(tool)
}

#[test]
fn compact_terminal_newlines_copy_original_source_across_split_and_repaint() {
    let mut driver = super::test_driver("copy-terminal-newlines");
    let script = "mkdir a\necho abc";
    for (rows, cols, sidebar) in [(24, 60, false), (34, 120, true), (34, 180, true)] {
        let mut app = App::for_test();
        app.sidebar = sidebar;
        let mut block = tool("terminal_use", "done");
        let Block::Tool(terminal) = &mut block else {
            unreachable!()
        };
        terminal.args = Some(serde_json::json!({"command":script}));
        app.blocks.push(block);
        let frame = draw::compose_for_test(&mut app, rows, cols);
        let first = point(&frame, "mkdir a ↵ echo abc");
        let mut last = point(&frame, "echo abc");
        last.col += "echo abc".len() as u16 - 1;
        assert_eq!(
            drag(&mut app, &mut driver, first, last, rows, cols),
            [script]
        );
        assert!(matches!(&app.blocks[0], Block::Tool(terminal) if !terminal.open));
    }
}

#[test]
fn drag_copy_excludes_sidebar_box_timeline_and_markdown_furniture_at_every_width() {
    let mut driver = super::test_driver("copy-surfaces");
    for (rows, cols) in [(18, 60), (34, 90), (46, 160), (36, 220)] {
        for (block, first, last, expected) in [
            (
                Block::Text {
                    text: "first-copy-line\nsecond-copy-line".into(),
                },
                "first-copy-line",
                "second-copy-line",
                "first-copy-line\nsecond-copy-line",
            ),
            (
                tool("terminal_use", "first-copy-line\nsecond-copy-line"),
                "first-copy-line",
                "second-copy-line",
                "first-copy-line\nsecond-copy-line",
            ),
            (
                tool("inspect_file", "first-copy-line\nsecond-copy-line"),
                "first-copy-line",
                "second-copy-line",
                "first-copy-line\nsecond-copy-line",
            ),
            (
                tool("create_file", "first-copy-line\nsecond-copy-line"),
                "first-copy-line",
                "second-copy-line",
                "first-copy-line\nsecond-copy-line",
            ),
            (
                Block::Text {
                    text: "```text\n  first-copy-line │ literal\n  second-copy-line\n```".into(),
                },
                "  first-copy-line",
                "  second-copy-line",
                "  first-copy-line │ literal\n  second-copy-line",
            ),
            (
                Block::Text {
                    text: "| first-copy-line | value |\n|---|---|\n| second-copy-line | next |"
                        .into(),
                },
                "first-copy-line",
                "second-copy-line",
                "first-copy-line\tvalue\nsecond-copy-line",
            ),
        ] {
            let mut app = App::for_test();
            app.sidebar = true;
            app.blocks.push(block);
            let frame = draw::compose_for_test(&mut app, rows, cols);
            let from = point(&frame, first);
            let mut to = point(&frame, last);
            to.col += ansi::width(last) as u16 - 1;
            let copies = drag(&mut app, &mut driver, from, to, rows, cols);
            assert_eq!(copies, vec![expected], "{rows}x{cols}, {first:?}");
        }
    }
    driver.engine.shutdown();
}

#[test]
fn composer_drag_copies_source_without_caret_move_and_click_still_places_it() {
    let mut driver = super::test_driver("copy-composer");
    for (rows, cols) in [(12, 40), (34, 120)] {
        let mut app = App::for_test();
        app.input = "alpha\nβ界\tend".into();
        app.cursor = app.input.len();
        draw::compose_for_test(&mut app, rows, cols);
        let row = |start| {
            app.hits
                .iter()
                .enumerate()
                .find_map(|(at, hit)| match hit {
                    Hit::ComposerText(index) if app.composer_hit_rows[*index].start == start => {
                        Some(at as u16 + 1)
                    }
                    _ => None,
                })
                .unwrap()
        };
        let from = Point {
            row: row(0),
            col: 5,
        };
        let to = Point {
            row: row(6),
            col: 12,
        };
        let original = app.input.clone();
        let cursor = app.cursor;
        assert_eq!(
            drag(&mut app, &mut driver, from, to, rows, cols),
            vec!["alpha\nβ界\t"]
        );
        assert_eq!(app.input, original);
        assert_eq!(app.cursor, cursor);
        assert!(app.input_editor.selected(&app.input, app.cursor).is_none());
        mouse(&mut app, &mut driver, Click::Press, from);
        mouse(&mut app, &mut driver, Click::Release, from);
        assert_eq!(app.cursor, 0);
        app.on_key(Key::Char('X'), &mut driver);
        assert_eq!(app.input, format!("X{original}"));
    }
    driver.engine.shutdown();
}

#[test]
fn models_input_drag_copies_in_both_directions_without_focusing_or_changing_draft() {
    let mut driver = super::test_driver("copy-models");
    for (rows, cols) in [(12, 40), (34, 120)] {
        let mut app = App::for_test();
        app.panel = Panel::Settings;
        app.preferences.navigate(settings::Page::Models);
        for key in "abcdef".chars() {
            app.on_key(Key::Char(key), &mut driver);
        }
        let frame = draw::compose_for_test(&mut app, rows, cols);
        let from = point(&frame, "abcdef");
        let to = Point {
            col: from.col + 3,
            ..from
        };
        for (first, last) in [(from, to), (to, from)] {
            assert_eq!(
                drag(&mut app, &mut driver, first, last, rows, cols),
                vec!["abc"]
            );
            assert!(app.input.is_empty());
        }
        app.on_key(Key::Char('X'), &mut driver);
        let frame = draw::compose_for_test(&mut app, rows, cols);
        assert!(frame
            .iter()
            .any(|line| ansi::plain(line).contains("abcdefX")));
    }
    driver.engine.shutdown();
}

#[test]
fn page_copy_never_crosses_into_sidebar_even_when_head_is_dragged_across_it() {
    let mut app = App::for_test();
    app.blocks.push(Block::Text {
        text: "left-one\nleft-two".into(),
    });
    let frame = draw::compose_for_test(&mut app, 34, 160);
    let from = point(&frame, "left-one");
    let to = point(&frame, "left-two");
    let selection = Selection {
        anchor: from,
        head: Point { col: 160, ..to },
        moved: true,
    };
    assert_eq!(
        selection.selected_text(&app.selection_rows),
        "left-one\nleft-two"
    );
    let side = app.sidebar_cols.unwrap().0;
    let selection = Selection {
        anchor: Point { row: 2, col: side },
        head: Point { row: 3, col: 160 },
        moved: true,
    };
    let copied = selection.selected_text(&app.selection_rows);
    assert!(
        !copied.contains("left-") && !copied.contains('│'),
        "{copied:?}"
    );
}

#[test]
fn all_readers_publish_content_ranges_and_never_copy_renderer_rules() {
    for panel in [
        Panel::Help,
        Panel::Context,
        Panel::Logs,
        Panel::Tasks,
        Panel::Undo,
        Panel::Redo,
        Panel::Capabilities,
    ] {
        let mut app = App::for_test();
        app.panel = panel;
        app.tasks = vec![serde_json::json!({"text":"task copy", "status":"todo"})];
        let doc = super::panels::document(&app, 100);
        assert_eq!(doc.sink.lines.len(), doc.sink.copy.len(), "{panel:?}");
        for (line, spans) in doc.sink.lines.iter().zip(&doc.sink.copy) {
            let plain = ansi::plain(line);
            if plain.trim().len() > 6 && plain.trim().chars().all(|c| c == '─') {
                assert!(spans.is_empty(), "{panel:?} rule enters copy: {line:?}");
            }
        }
        let frame = draw::compose_for_test(&mut app, 46, 160);
        assert_eq!(frame.len(), app.selection_rows.len());
    }
}

#[test]
fn resizing_or_replacing_selected_content_cancels_drag_and_swallows_release() {
    let mut driver = super::test_driver("copy-cancel");
    for resize in [false, true] {
        let mut app = App::for_test();
        app.blocks.push(Block::Text {
            text: "copy-selected".into(),
        });
        let frame = draw::compose_for_test(&mut app, 34, 120);
        let first = point(&frame, "copy-selected");
        let last = Point {
            col: first.col + 6,
            ..first
        };
        term::take_copies();
        mouse(&mut app, &mut driver, Click::Press, first);
        mouse(&mut app, &mut driver, Click::Drag, last);
        if resize {
            draw::compose_for_test(&mut app, 34, 160);
        } else {
            app.blocks[0] = Block::Text {
                text: "replacement".into(),
            };
            app.transcript_dirty = true;
            draw::compose_for_test(&mut app, 34, 120);
        }
        assert!(app.selection.is_none());
        assert!(app.selection_cancelled);
        mouse(&mut app, &mut driver, Click::Release, last);
        assert!(term::take_copies().is_empty());
        assert!(!app.selection_cancelled);
    }
    driver.engine.shutdown();
}

#[test]
fn dragging_blank_chrome_does_not_open_a_tool() {
    let mut driver = super::test_driver("copy-chrome-no-click");
    let mut app = App::for_test();
    app.blocks.push(tool("terminal_use", "result"));
    let frame = draw::compose_for_test(&mut app, 34, 120);
    let from = point(&frame, "╭─ Bash");
    let to = Point {
        col: from.col + 10,
        ..from
    };
    for (kind, p) in [
        (Click::Press, from),
        (Click::Drag, to),
        (Click::Release, to),
    ] {
        mouse(&mut app, &mut driver, kind, p);
    }
    assert!(term::take_copies().is_empty());
    assert!(matches!(&app.blocks[0], Block::Tool(tool) if !tool.open));
    assert!(app.selection.is_none());
    assert!(app.hits.iter().any(|hit| matches!(hit, Hit::Block(0))));
    driver.engine.shutdown();
}
