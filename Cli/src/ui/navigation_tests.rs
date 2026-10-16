//! These tests use the real dispatcher and renderer together: a key is right
//! only when the state it changes belongs to the surface the person can see.
use super::{
    draw,
    input::{Click, Key, Modifiers, Mouse},
    settings, App, Awaiting, Panel,
};
use serde_json::json;

fn questions(app: &mut App) {
    app.on_event(runeengine::Event::QuestionsRequest {
        session: 1, id: "call".into(), req: "request".into(), action: "Choose".into(),
        questions: serde_json::from_value(json!([{"id":"q","title":"Q","question":"Pick","type":"single",
            "options":[{"id":"a","label":"Alpha","description":"First"},{"id":"b","label":"Beta","description":"Second"}]}])).unwrap(),
    });
}
fn wheel(up: bool, row: u16, col: u16, ctrl: bool) -> Key {
    Key::Mouse(Mouse {
        kind: if up { Click::WheelUp } else { Click::WheelDown },
        row,
        col,
        modifiers: Modifiers { ctrl, alt: false },
    })
}
fn app(form: u8) -> App {
    let mut app = App::for_test();
    app.sidebar = false;
    app.terminal_keeps_up = false;
    app.logs = (0..80)
        .map(|n| runeengine::proto::LogEntry {
            at: n,
            level: runeengine::proto::LogLevel::Info,
            source: "test".into(),
            code: "navigation".into(),
            message: format!("message {n}"),
            turn: None,
            detail: Some(json!({"text":"detail"})),
        })
        .collect();
    app.tasks = (0..80)
        .map(|n| json!({"text":format!("step {n}"),"status":"todo"}))
        .collect();
    app.file_changes = (0..80)
        .map(|n| json!({"path":format!("file-{n}.rs"),"added":1,"removed":0}))
        .collect();
    app.redo_files = app.file_changes.clone();
    app.context_view = json!({"system":(0..150).map(|n|format!("context {n}")).collect::<Vec<_>>().join("\n"),"tools":[],"messages":[]});
    if form == 1 {
        questions(&mut app);
    }
    if form == 2 {
        app.mcp_input = Some(super::mcp_input::Form::new(
            "mcp".into(),
            "server".into(),
            json!({"mode":"form","requestedSchema":{"type":"object","properties":{"name":{"type":"string"}}}}),
        ));
    }
    app
}

#[test]
fn settings_and_all_overlays_own_navigation_with_and_without_pending_forms() {
    let mut d = super::test_driver("navigation-ownership");
    for form in 0..=2 {
        for page in [
            settings::Page::Overview,
            settings::Page::Runtime,
            settings::Page::Tools,
            settings::Page::Appearance,
        ] {
            let mut a = app(form);
            settings::enter(&mut a, page.clone(), &mut d);
            draw::compose_for_test(&mut a, 30, 120);
            let before = a.preferences.location.row;
            a.on_key(wheel(false, 12, 10, false), &mut d);
            assert!(a.preferences.location.row > before, "{page:?}, form={form}");
            let at = a.preferences.location.row;
            a.on_key(Key::Up, &mut d);
            assert!(a.preferences.location.row < at);
            assert_eq!(a.scroll, 0, "Settings wheel leaked to chat");
        }
        for panel in [Panel::Logs, Panel::Undo, Panel::Redo] {
            let mut a = app(form);
            a.show_panel(panel);
            draw::compose_for_test(&mut a, 30, 120);
            a.on_key(Key::Down, &mut d);
            assert_eq!(a.panel_row, 1, "{panel:?} form={form}");
            a.on_key(wheel(false, 5, 10, false), &mut d);
            assert_eq!(a.panel_row, 2);
            a.on_key(Key::PageDown, &mut d);
            assert_eq!(a.panel_row, 7);
            assert_eq!(a.scroll, 0);
            if panel == Panel::Logs {
                a.on_key(Key::Enter, &mut d);
                assert_eq!(a.log_open, Some(7));
            }
        }
        for panel in [
            Panel::Context,
            Panel::Help,
            Panel::Tasks,
            Panel::Capabilities,
        ] {
            let mut a = app(form);
            a.show_panel(panel);
            if panel == Panel::Capabilities {
                a.capability_view = json!({"content":"line\n".repeat(100)});
            }
            draw::compose_for_test(&mut a, 30, 120);
            a.panel_view.scroll_by(true, usize::MAX);
            draw::compose_for_test(&mut a, 30, 120);
            a.on_key(Key::Down, &mut d);
            let first = a.panel_view.scroll;
            a.on_key(wheel(false, 5, 10, false), &mut d);
            assert!(a.panel_view.scroll < first, "{panel:?} form={form}");
            assert_eq!(a.scroll, 0);
            if form > 0 {
                assert!(
                    super::panels::key(&mut a, &Key::Enter, &mut d),
                    "reader Enter must not reach waiting form"
                );
            }
        }
    }
    d.engine.shutdown();
}

#[test]
fn every_list_keeps_its_chevron_visible_through_keyboard_wheel_and_reflow() {
    let mut d = super::test_driver("navigation-chevron");
    for form in 0..=1 {
        for panel in [Panel::Logs, Panel::Undo, Panel::Redo] {
            let mut a = app(form);
            a.show_panel(panel);
            for cols in [120, 50, 120] {
                for key in [
                    Key::End,
                    Key::Up,
                    Key::Home,
                    Key::Down,
                    wheel(false, 5, 10, false),
                ] {
                    draw::compose_for_test(&mut a, 30, cols);
                    a.on_key(key, &mut d);
                    let rows = draw::compose_for_test(&mut a, 30, cols);
                    let hit = if panel == Panel::Logs {
                        draw::Hit::Log(a.panel_row)
                    } else {
                        *a.hits.iter().find(|h|matches!(h,draw::Hit::UndoFile{index,..} if *index==a.panel_row)).expect("selected file visible")
                    };
                    let at = a
                        .hits
                        .iter()
                        .position(|h| *h == hit)
                        .expect("selected log visible");
                    assert!(
                        rows.iter().any(|r| super::ansi::plain(r).contains('›')),
                        "{panel:?} has no visible chevron at {at}"
                    );
                }
            }
        }
    }
    d.engine.shutdown();
}

#[test]
fn visible_palette_owns_choice_confirm_and_escape_before_every_overlay() {
    let mut d = super::test_driver("navigation-palette");
    for panel in [
        Panel::Logs,
        Panel::Context,
        Panel::Undo,
        Panel::Redo,
        Panel::Help,
        Panel::Tasks,
        Panel::Capabilities,
    ] {
        let mut a = app(0);
        a.show_panel(panel);
        a.input = "/".into();
        a.cursor = 1;
        draw::compose_for_test(&mut a, 30, 120);
        a.on_key(Key::Down, &mut d);
        assert_eq!(a.palette_row, 1);
        assert_eq!(a.panel_row, 0);
        a.on_key(Key::Home, &mut d);
        assert_eq!(a.cursor, 0);
        assert_eq!(a.panel_row, 0);
        a.on_key(Key::End, &mut d);
        assert_eq!(a.cursor, 1);
        assert_eq!(a.panel_row, 0);
        a.on_key(Key::Esc, &mut d);
        assert!(a.input.is_empty());
        assert_eq!(a.panel, panel);
        a.input = "/help".into();
        a.cursor = 5;
        a.on_key(Key::Enter, &mut d);
        assert_eq!(a.panel, Panel::Help);
        assert!(a.input.is_empty());
        a.on_key(Key::Esc, &mut d);
        assert_eq!(a.panel, Panel::None);
    }
    let mut a = app(1);
    a.input = "/".into();
    a.cursor = 1;
    assert!(!a.searching(), "hidden composer must not own palette");
    let mode = a.mode.clone();
    a.on_key(Key::Tab, &mut d);
    assert_eq!(a.mode, mode);
    assert_eq!(a.input, "/");
    d.engine.shutdown();
}

#[test]
fn optional_view_arguments_do_not_block_or_stack_screen_commands() {
    let mut d = super::test_driver("navigation-optional-commands");
    let mut a = app(0);
    for (command, panel) in [
        ("/logs", Panel::Logs),
        ("/context", Panel::Context),
        ("/logs warn", Panel::Logs),
        ("/help", Panel::Help),
        ("/undo", Panel::Undo),
        ("/redo", Panel::Redo),
    ] {
        a.input = command.into();
        a.cursor = a.input.len();
        a.on_key(Key::Enter, &mut d);
        assert!(
            a.input.is_empty(),
            "command {command} was only completed, not executed"
        );
        assert_eq!(a.panel, panel);
    }
    a.on_key(Key::Esc, &mut d);
    assert_eq!(a.panel, Panel::None);
    d.engine.shutdown();
}

#[test]
fn replacing_or_closing_a_screen_discards_all_old_presentation_responses() {
    let mut a = app(0);
    for what in [
        Awaiting::Context,
        Awaiting::Logs,
        Awaiting::Capabilities,
        Awaiting::Sessions,
        Awaiting::UndoFiles {
            side: super::undo::Side::Undo,
        },
        Awaiting::UndoDiff {
            file: "file".into(),
            side: super::undo::Side::Redo,
        },
    ] {
        a.awaiting.insert(91, what);
        a.show_panel(Panel::Help);
        assert!(!a.awaiting.contains_key(&91));
        a.on_event(runeengine::Event::Ack {
            id: 91,
            ok: true,
            error: None,
            data: Some(json!({"entries":[],"content":"old","sessions":[]})),
        });
        assert_eq!(a.panel, Panel::Help);
        assert!(a.session_picker.is_none());
    }
    let mut d = super::test_driver("navigation-pending-picker-escape");
    a.show_panel(Panel::None);
    a.awaiting.insert(93, Awaiting::Sessions);
    a.on_key(Key::Esc, &mut d);
    a.on_event(runeengine::Event::Ack {
        id: 93,
        ok: true,
        error: None,
        data: Some(json!({"sessions":[]})),
    });
    assert!(a.session_picker.is_none());
    d.engine.shutdown();
    a.awaiting.insert(92, Awaiting::Compact);
    a.show_panel(Panel::None);
    assert!(
        a.awaiting.contains_key(&92),
        "navigation must never discard operation ACKs"
    );
}

#[test]
fn fresh_drafts_equal_to_history_restore_whole_and_never_guess_their_origin() {
    let mut a = app(0);
    a.prompt_history = vec!["old".into(), "same".into()];
    a.input = "same".into();
    a.cursor = 0;
    a.history_prev();
    assert_eq!(a.input, "same");
    assert_eq!(a.history_index, Some(1));
    assert_eq!(a.history_draft, "same");
    a.history_prev();
    assert_eq!(a.input, "old");
    a.history_next();
    assert_eq!(a.input, "same");
    a.history_next();
    assert_eq!(a.input, "same");
    assert_eq!(a.history_index, None);
    a.history_next();
    assert_eq!(a.input, "same", "unindexed text is never history");
}

#[test]
fn modified_wheels_equal_three_plain_notches_on_every_scrollable_surface() {
    let mut d = super::test_driver("navigation-fast-wheel");
    for form in 0..=2 {
        for panel in [
            Panel::Logs,
            Panel::Undo,
            Panel::Redo,
            Panel::Context,
            Panel::Help,
            Panel::Tasks,
            Panel::Capabilities,
            Panel::Settings,
        ] {
            for up in [false, true] {
                let mut outcomes = Vec::new();
                for fast in [false, true] {
                    let mut a = app(form);
                    a.show_panel(panel);
                    if panel == Panel::Settings {
                        settings::enter(&mut a, settings::Page::Tools, &mut d);
                    }
                    if panel == Panel::Capabilities {
                        a.capability_view = json!({"content":"line\n".repeat(100)});
                    }
                    draw::compose_for_test(&mut a, 30, 120);
                    if panel == Panel::Settings {
                        for _ in 0..8 {
                            a.on_key(wheel(false, 5, 10, false), &mut d);
                        }
                    } else if matches!(panel, Panel::Logs | Panel::Undo | Panel::Redo) {
                        a.panel_row = 30;
                        a.panel_view.reveal();
                    } else {
                        a.panel_view.scroll_by(true, 10);
                    }
                    draw::compose_for_test(&mut a, 30, 120);
                    for _ in 0..if fast { 1 } else { 3 } {
                        a.on_key(wheel(up, 5, 10, fast), &mut d);
                        draw::compose_for_test(&mut a, 30, 120);
                    }
                    outcomes.push((
                        a.panel_row,
                        a.panel_view.scroll,
                        a.preferences.location.row,
                        a.scroll,
                        a.mode.clone(),
                        a.input.clone(),
                    ));
                }
                assert_eq!(outcomes[0], outcomes[1], "{panel:?} form={form} up={up}");
            }
        }
    }
    for alt in [false, true] {
        let modifiers = super::input::Modifiers { ctrl: !alt, alt };
        assert_eq!(modifiers.wheel_multiplier(), 3);
        let mut a = app(0);
        a.input = "/".into();
        a.cursor = 1;
        let mut gesture = wheel(false, 1, 1, false);
        draw::compose_for_test(&mut a, 30, 120);
        let row = a
            .hits
            .iter()
            .position(|h| matches!(h, draw::Hit::Palette(_)))
            .unwrap() as u16
            + 1;
        if let Key::Mouse(m) = &mut gesture {
            m.row = row;
            m.modifiers = modifiers;
        }
        a.on_key(gesture, &mut d);
        assert_eq!(a.palette_row, 3);
        let items = (0..20)
            .map(|n| super::picker::SessionItem {
                id: n,
                title: format!("Session {n}"),
                project: "/project".into(),
                created_at: 1,
                updated_at: 1,
                active_seconds: 1,
                model: None,
                size_bytes: 1,
            })
            .collect();
        a.input.clear();
        a.session_picker = Some(super::picker::SessionPicker::new(items, None));
        draw::compose_for_test(&mut a, 30, 120);
        let mut gesture = wheel(false, 10, 10, false);
        if let Key::Mouse(m) = &mut gesture {
            m.modifiers = modifiers;
        }
        a.on_key(gesture, &mut d);
        assert_eq!(a.session_picker.as_ref().unwrap().selected, 3);
    }
    d.engine.shutdown();
}

#[test]
fn fast_wheels_stay_on_long_cards_and_sidebar_even_at_their_ends() {
    let mut d = super::test_driver("navigation-fast-card");
    for surface in 0..3 {
        for up in [false, true] {
            let mut outcomes = Vec::new();
            for fast in [false, true] {
                let mut a = app(0);
                if surface == 0 {
                    a.on_event(runeengine::Event::QuestionsRequest{session:1,id:"call".into(),req:"long".into(),action:"Pick".into(),questions:serde_json::from_value(json!([{"id":"q","title":"Long","question":"Pick","type":"single","options":(0..100).map(|n|json!({"id":format!("o{n}"),"label":format!("Option {n}"),"description":"Details"})).collect::<Vec<_>>()}])).unwrap()});
                } else if surface == 1 {
                    a.pending = Some(super::blocks::Ask {
                        req: "permission".into(),
                        action: "Run".into(),
                        preview: "output\n".repeat(200),
                        remember_as: None,
                        reach: Vec::new(),
                        sensitive: Vec::new(),
                        full_access: false,
                        reason: None,
                    });
                } else {
                    a.sidebar = true;
                }
                draw::compose_for_test(&mut a, 30, 150);
                let (row, col) = if surface == 0 {
                    (a.questions_card.unwrap().0 + 2, 10)
                } else if surface == 1 {
                    (a.permission.card.unwrap().0 + 2, 10)
                } else {
                    (4, a.sidebar_cols.unwrap().0 + 1)
                };
                // Move well inside before comparing both directions.
                for _ in 0..2 {
                    a.on_key(wheel(false, row, col, false), &mut d);
                    draw::compose_for_test(&mut a, 30, 150);
                }
                for _ in 0..if fast { 1 } else { 3 } {
                    a.on_key(wheel(up, row, col, fast), &mut d);
                    draw::compose_for_test(&mut a, 30, 150);
                }
                outcomes.push((
                    a.plan_scroll,
                    a.permission.scroll,
                    a.scroll,
                    a.questions_card,
                    draw::compose_for_test(&mut a, 30, 150),
                ));
                assert_eq!(a.ask_choice, 0);
                assert_eq!(a.scroll, 0);
            }
            assert_eq!(outcomes[0], outcomes[1], "surface {surface} up={up}");
        }
    }
    d.engine.shutdown();
}

#[test]
fn a_palette_never_takes_wheel_or_clicks_from_the_sidebar_beside_it() {
    let mut d = super::test_driver("navigation-palette-sidebar");
    let mut a = app(0);
    a.sidebar = true;
    a.input = "/".into();
    a.cursor = 1;
    draw::compose_for_test(&mut a, 30, 150);
    let row = a
        .hits
        .iter()
        .position(|h| matches!(h, draw::Hit::Palette(_)))
        .unwrap() as u16
        + 1;
    let col = a.sidebar_cols.unwrap().0 + 1;
    a.plan_scroll = a.plan_max;
    let before = a.plan_scroll;
    a.on_key(wheel(true, row, col, true), &mut d);
    assert_eq!(a.palette_row, 0);
    assert_eq!(a.plan_scroll, before.saturating_sub(9));
    for kind in [Click::Press, Click::Release] {
        a.on_key(
            Key::Mouse(Mouse {
                kind,
                row,
                col,
                modifiers: Modifiers::default(),
            }),
            &mut d,
        );
    }
    assert_eq!(a.input, "/");
    d.engine.shutdown();
}

#[test]
fn full_screen_settings_discards_old_sidebar_hit_geometry() {
    let mut a = app(1);
    a.sidebar = true;
    draw::compose_for_test(&mut a, 30, 150);
    assert!(a.sidebar_cols.is_some());
    a.show_panel(Panel::Settings);
    draw::compose_for_test(&mut a, 30, 150);
    assert!(a.sidebar_cols.is_none());
    assert_eq!(a.sidebar_last_row, 0);
}

#[test]
fn official_help_covers_question_boundaries_palette_and_screen_ownership() {
    let shortcuts = runeengine::capabilities::product::document("cli/shortcuts.md").unwrap();
    for phrase in [
        "Alt+Left/Right",
        "Empty answers",
        "slash-command palette",
        "late reply",
        "selection and viewport",
        "Tab/Shift+Tab",
    ] {
        assert!(
            shortcuts.contains(phrase),
            "missing navigation help: {phrase}"
        );
    }
    let questions =
        runeengine::capabilities::product::document("tools/ask-user-questions.md").unwrap();
    assert!(
        questions.contains("Alt+Left/Right")
            && questions.contains("without changing conversation mode")
    );
}

#[test]
fn promotion_keeps_the_first_prompt_and_engine_echoes_do_not_duplicate_history() {
    let mut a = app(0);
    a.composition.submitted = Some(runeengine::store::composer::Composer {
        text: "first".into(),
        ..Default::default()
    });
    a.awaiting.insert(17, Awaiting::Promoted);
    a.on_event(runeengine::Event::Ack {
        id: 17,
        ok: true,
        error: None,
        data: Some(json!({"session":1,"promotion":"token"})),
    });
    assert_eq!(a.prompt_history, vec!["first"]);
    for text in ["first", "remote"] {
        a.on_event(runeengine::Event::TurnStart {
            session: 1,
            turn: 1,
            text: Some(text.into()),
            notification: false,
            media: Vec::new(),
            at: 1,
        });
    }
    assert_eq!(a.prompt_history, vec!["first", "remote"]);
}

#[test]
fn sidebar_and_modified_chat_wheels_ignore_the_presence_of_a_question() {
    let mut d = super::test_driver("navigation-wheel-parity");
    let mut amounts = Vec::new();
    for form in 0..=1 {
        let mut a = app(form);
        a.sidebar = true;
        draw::compose_for_test(&mut a, 30, 150);
        let (first, _) = a.sidebar_cols.unwrap();
        a.plan_scroll = a.plan_max;
        let at = a.plan_scroll;
        a.on_key(wheel(true, 4, first + 1, false), &mut d);
        assert_eq!(a.plan_scroll, at.saturating_sub(3));
        assert_eq!(a.scroll, 0);
        a.drawn_rows = 500;
        a.on_key(wheel(true, 4, 5, true), &mut d);
        amounts.push(a.scroll);
    }
    assert_eq!(amounts[0], amounts[1]);
    d.engine.shutdown();
}
