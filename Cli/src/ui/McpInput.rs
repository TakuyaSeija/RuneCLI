//! Schema-driven MCP input with the existing editor, theme and form broker.
use super::{ansi, draw::Sink, input::Key, App};
use crate::driver::Driver;
use runeengine::{capabilities::input::Reply, Command};
use serde_json::{json, Value};

pub struct Form {
    pub req: String,
    server: String,
    request: Value,
    fields: Vec<(String, Value)>,
    values: Vec<String>,
    cursors: Vec<usize>,
    editors: Vec<super::editing::State>,
    selected: usize,
    text_row: Option<(usize, super::editor::Row)>,
    pub sending: Option<u64>,
    pub error: Option<String>,
}
impl Form {
    pub fn new(req: String, server: String, request: Value) -> Self {
        let fields: Vec<_> = request["requestedSchema"]["properties"]
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        let values: Vec<_> = fields
            .iter()
            .map(|(_, s)| {
                s.get("default")
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| v.to_string())
                    })
                    .unwrap_or_default()
            })
            .collect();
        let cursors = values.iter().map(String::len).collect();
        let editors = vec![super::editing::State::default(); values.len()];
        Self {
            req,
            server,
            request,
            fields,
            values,
            cursors,
            editors,
            selected: 0,
            text_row: None,
            sending: None,
            error: None,
        }
    }
    fn reply(&self) -> Result<Reply, String> {
        if self.request["mode"] == "url" {
            return Ok(Reply {
                action: "accept".into(),
                content: None,
            });
        }
        let mut value = json!({});
        for ((name, schema), text) in self.fields.iter().zip(&self.values) {
            if text.is_empty() {
                continue;
            }
            value[name] = match schema["type"].as_str() {
                Some("string") => json!(text),
                Some("boolean" | "integer" | "number" | "array") => serde_json::from_str(text)
                    .map_err(|_| {
                        format!(
                            "Enter a valid {} for {}",
                            schema["type"].as_str().unwrap_or("value"),
                            name
                        )
                    })?,
                _ => return Err("This form field type is unsupported".into()),
            };
        }
        let reply = Reply {
            action: "accept".into(),
            content: Some(value),
        };
        use runeengine::perm::broker::Response;
        if !reply.valid(&self.request) {
            return Err("Complete required fields and check the allowed values".into());
        }
        Ok(reply)
    }
}

pub fn key(app: &mut App, key: &Key, driver: &mut Driver) {
    let Some(form) = app.mcp_input.as_mut() else {
        return;
    };
    if form.sending.is_some() {
        return;
    }
    let reply = match key {
        Key::Esc | Key::Ctrl('c') => Some(Reply {
            action: "cancel".into(),
            content: None,
        }),
        Key::Tab | Key::Down => {
            form.selected = (form.selected + 1) % (form.fields.len() + 1);
            None
        }
        Key::ShiftTab | Key::Up => {
            form.selected = if form.selected == 0 {
                form.fields.len()
            } else {
                form.selected - 1
            };
            None
        }
        Key::Enter if form.selected == form.fields.len() => match form.reply() {
            Ok(reply) => Some(reply),
            Err(e) => {
                form.error = Some(e);
                None
            }
        },
        Key::Enter => {
            form.selected += 1;
            None
        }
        Key::Char(' ')
            if form.selected < form.fields.len()
                && form.fields[form.selected].1["type"] == "boolean" =>
        {
            let value = &mut form.values[form.selected];
            *value = if value == "true" { "false" } else { "true" }.into();
            form.cursors[form.selected] = value.len();
            None
        }
        _ => {
            if form.selected < form.fields.len() {
                let index = form.selected;
                let mut value = form.values[index].clone();
                let mut cursor = form.cursors[index];
                if form.editors[index].key(
                    &mut value,
                    &mut cursor,
                    key,
                    super::editing::Options::single(4096, 4096),
                ) {
                    form.values[index] = value;
                    form.cursors[index] = cursor;
                    form.error = None;
                }
            }
            None
        }
    };
    if let Some(reply) = reply {
        let req = form.req.clone();
        let Some(session) = app.session else {
            return;
        };
        form.sending = Some(driver.ask(|id| Command::McpInputReply {
            id,
            session,
            req,
            reply,
        }));
    }
}

pub fn submit(app: &mut App, driver: &mut Driver) {
    if let Some(form) = app.mcp_input.as_mut() {
        form.selected = form.fields.len();
    }
    key(app, &Key::Enter, driver);
}

pub fn click(app: &mut App, field: usize, col: u16) {
    let Some(form) = app.mcp_input.as_mut().filter(|f| f.sending.is_none()) else {
        return;
    };
    let Some((index, geometry)) = form.text_row.as_ref() else {
        return;
    };
    if *index != field || form.selected != field {
        return;
    }
    let at = geometry.caret_at(col);
    if form.values[field].is_char_boundary(at) {
        form.editors[field].place(&form.values[field], &mut form.cursors[field], at);
    }
}

pub(super) fn select(app: &mut App, field: usize, first: u16, last: u16) -> bool {
    let Some(form) = app.mcp_input.as_mut().filter(|f| f.sending.is_none()) else {
        return false;
    };
    let Some((index, row)) = form.text_row.as_ref() else {
        return false;
    };
    if *index != field {
        return false;
    }
    let (start, end) = (row.caret_at(first), row.caret_at(last));
    form.editors[field].select(&form.values[field], &mut form.cursors[field], start, end);
    true
}

pub fn render(form: &mut Form, width: usize, height: usize) -> (Sink, Option<(usize, usize)>) {
    form.text_row = None;
    let height = super::viewport::card_rows(height, 5, height);
    let mut out = Sink::default();
    let inner = width.saturating_sub(4).max(1);
    out.push(format!(
        "  {}MCP · {}{}",
        ansi::ACCENT,
        ansi::plain(&form.server),
        ansi::RESET
    ));
    for line in ansi::wrap(
        &ansi::plain(
            form.request["message"]
                .as_str()
                .unwrap_or("Additional information requested"),
        ),
        inner,
    )
    .into_iter()
    .take(height.saturating_sub(6).min(2))
    {
        out.push(format!("  {line}"));
    }
    let mut cursor = None;
    if form.request["mode"] == "url" {
        if let Some(url) = form.request["url"].as_str() {
            out.push(format!("  {}", ansi::link(url, "Open requested page")));
        }
        out.push("  Complete the browser step, then confirm below.".into());
    } else if let Some((name, schema)) = form.fields.get(form.selected) {
        let title = schema["title"].as_str().unwrap_or(name);
        out.push(format!(
            "  {} · field {}/{}",
            ansi::truncate(&ansi::plain(title), inner.saturating_sub(18)),
            form.selected + 1,
            form.fields.len()
        ));
        if let Some(values) = schema["enum"].as_array() {
            out.push(format!(
                "  Allowed: {}",
                ansi::truncate(
                    &values
                        .iter()
                        .map(|v| v
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| v.to_string()))
                        .collect::<Vec<_>>()
                        .join(", "),
                    inner.saturating_sub(9)
                )
            ));
        }
        let projection = super::editor::Projection::single_line(&form.values[form.selected]);
        let (geometry, col) = projection.window(form.cursors[form.selected], inner, 3);
        cursor = Some((out.lines.len(), col));
        out.push_at(
            format!(
                "  {}",
                geometry.styled(
                    form.editors[form.selected]
                        .selected(&form.values[form.selected], form.cursors[form.selected])
                )
            ),
            super::draw::Hit::McpText(form.selected),
        );
        form.text_row = Some((form.selected, geometry));
    }
    out.push_at(
        format!(
            "  {}Submit{} · {} fields",
            if form.selected == form.fields.len() {
                ansi::ACCENT
            } else {
                ansi::MUTED
            },
            ansi::RESET,
            form.fields.len()
        ),
        super::draw::Hit::McpSubmit { first: 3, last: 8 },
    );
    if let Some(error) = &form.error {
        out.push(format!(
            "  {}{}{}",
            ansi::WARN,
            ansi::truncate(error, inner),
            ansi::RESET
        ));
    }
    out.push(ansi::truncate(
        "  Tab/↑↓ fields · Enter next/submit · Esc cancel",
        width,
    ));
    if out.lines.len() > height {
        out.lines.truncate(height);
        out.owner.truncate(height);
        cursor = cursor.filter(|(row, _)| *row < height);
    }
    (out, cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_editing_matrix_is_field_local_and_never_submits_mcp() {
        let mut driver = super::super::test_driver("mcp-editing-matrix");
        for case in super::super::editing::cases() {
            let mut app = App::for_test();
            let mut form = Form::new(
                "req".into(),
                "server".into(),
                json!({"mode":"form","requestedSchema":{"type":"object","properties":{"answer":{"type":"string"},"other":{"type":"string"}}}}),
            );
            form.values[0] = case.text.into();
            form.cursors[0] = case.at;
            app.mcp_input = Some(form);
            for key in case.keys {
                app.on_key(key, &mut driver);
            }
            let form = app.mcp_input.as_ref().unwrap();
            assert_eq!(form.values[0], case.expected, "{}", case.name);
            assert!(form.values[1].is_empty());
            assert!(form.sending.is_none());
            assert_eq!(app.panel, super::super::Panel::None);
        }
        driver.engine.shutdown();
    }

    #[test]
    fn a_real_mouse_click_moves_mcp_caret_without_submitting_or_typing_escapes() {
        use super::super::input::{Click, Modifiers, Mouse};
        let mut app = App::for_test();
        let mut driver = super::super::test_driver("mcp-caret-mouse");
        let request = json!({"mode":"form","requestedSchema":{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]}});
        let mut form = Form::new("mcp-text".into(), "server".into(), request);
        let text = "a\té界e\u{301}👨\u{200d}💻\x1b[31mend\x1b[0m";
        form.values[0] = text.into();
        form.cursors[0] = text.len();
        app.mcp_input = Some(form);
        for width in [20, 32, 100] {
            let frame = super::super::draw::compose_for_test(&mut app, 30, width);
            let screen = app
                .hits
                .iter()
                .position(|hit| *hit == super::super::draw::Hit::McpText(0))
                .unwrap();
            assert!(
                !frame[screen].contains("\x1b[31m"),
                "input escapes cannot become styles"
            );
            let expected = app
                .mcp_input
                .as_ref()
                .unwrap()
                .text_row
                .as_ref()
                .unwrap()
                .1
                .caret_at(4);
            for kind in [Click::Press, Click::Release] {
                app.on_key(
                    Key::Mouse(Mouse {
                        kind,
                        row: screen as u16 + 1,
                        col: 4,
                        modifiers: Modifiers::NONE,
                    }),
                    &mut driver,
                );
            }
            let form = app.mcp_input.as_ref().unwrap();
            assert_eq!(form.cursors[0], expected);
            assert_eq!(form.values[0], text);
            assert!(form.sending.is_none());
        }
        driver.engine.shutdown();
    }

    #[test]
    fn schema_fields_require_real_complete_answers_and_fit_small_terminals() {
        let request = json!({"mode":"form","message":"Choose the account details.","requestedSchema":{"type":"object","properties":{"name":{"type":"string","minLength":2},"count":{"type":"integer","minimum":1}},"required":["name","count"],"additionalProperties":false}});
        let mut form = Form::new("request".into(), "test server".into(), request);
        assert!(form.reply().is_err());
        for (index, (name, _)) in form.fields.iter().enumerate() {
            form.values[index] = if name == "name" {
                "Ada".into()
            } else {
                "2".into()
            };
            form.cursors[index] = form.values[index].len();
        }
        assert_eq!(form.reply().unwrap().content.unwrap()["name"], "Ada");
        let (sink, cursor) = render(&mut form, 24, 5);
        assert!(sink.lines.len() <= 5);
        assert!(cursor.is_some());
        form.selected = form.fields.len();
        let (sink, _) = render(&mut form, 24, 5);
        assert!(sink.lines.iter().any(|s| s.contains("Submit")));
    }
}
