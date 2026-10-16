//! The overlays: settings, the plan, undo history, and help.
//!
//! Each one takes the whole content area rather than floating over it. A half
//! transparent panel on top of a live transcript is two things competing for
//! the same rows, and the transcript always loses.
//!
//! The settings panel is generated from the engine's own knob list — ranges,
//! labels and help text all arrive over the protocol — so a setting added to the
//! engine appears here with its real bounds and cannot drift out of step with
//! them. What it does *not* do is invent a value: a knob the current provider
//! ignores is shown greyed with the reason, never as though it were in force.

use serde_json::Value;

use super::ansi::{self, ACCENT, FAINT, MUTED, OK, RESET, RULE, TEXT, WARN};
use super::draw::Sink;
use super::input::Key;
use super::{App, Panel};
use crate::driver::Driver;

pub fn document(app: &App, width: usize) -> super::panel_view::Document {
    let mut doc = match app.panel {
        // Its keys head the panel, where the list cannot scroll them away.
        Panel::Undo | Panel::Redo => return super::undo::document(app, width),
        Panel::Context => context_document(app, width),
        Panel::Logs => logs_document(app, width),
        _ => return super::panel_view::Document::plain(render(app, width)),
    };
    doc.append("hint", vec![String::new()]);
    doc.append("hint:body", paragraph(&hint(app), FAINT, 2, width));
    doc
}

pub fn render(app: &App, width: usize) -> Sink {
    let mut out = Sink::default();
    match app.panel {
        // Its own module: it is a screen rather than a panel, and it had grown
        // to more code than the other three put together.
        Panel::Settings => return super::settings::render(app, width, 32).0,
        Panel::Tasks => out.extend(tasks(app, width)),
        Panel::Undo | Panel::Redo => return super::undo::document(app, width).sink,
        Panel::Logs => out.extend(logs(app, width)),
        Panel::Context => out.extend(context(app, width)),
        Panel::Capabilities => {
            out.extend(title(
                "CAPABILITIES",
                "Skills and MCP connections available to Rune",
                width,
            ));
            let data = &app.capability_view;
            for key in ["skills", "servers", "prompts"] {
                if let Some(entries) = data[key].as_array() {
                    for entry in entries {
                        let name = entry["name"]
                            .as_str()
                            .or(entry["id"].as_str())
                            .unwrap_or("Capability");
                        let state =
                            entry["status"]
                                .as_str()
                                .unwrap_or(if entry["disabled"] == true {
                                    "disabled"
                                } else {
                                    "available"
                                });
                        out.push(format!(
                            "  {ACCENT}{}{} · {MUTED}{}{}",
                            ansi::plain(name),
                            ansi::RESET,
                            ansi::plain(state),
                            ansi::RESET
                        ));
                        if let Some(id) = entry["id"].as_str() {
                            out.extend(ansi::wrap(&format!("    {}", ansi::plain(id)), width));
                        }
                        if let Some(desc) = entry["description"].as_str() {
                            out.extend(ansi::wrap(&format!("    {}", ansi::plain(desc)), width));
                        }
                        out.push(String::new());
                    }
                    if entries.is_empty() {
                        out.push(format!("  {MUTED}No {key} available{}", ansi::RESET));
                    }
                }
            }
            if let Some(content) = data["content"].as_str() {
                out.extend(super::markdown::render(content, width));
            }
            if let Some(url) = data["authorization_url"].as_str() {
                out.extend(super::markdown::render(&format!("[Open sign-in page]({url})\n\nComplete sign-in in your browser, then use /mcp login-status <id>."),width));
            }
            if let Some(status) = data["status"].as_str() {
                out.push(format!("  {}", ansi::plain(status)));
            }
            if data["reload_required"] == true {
                out.push("  Saved. Use /skills reload to refresh the skill catalog.".into());
            }
            if data["approval_required"] == true {
                out.push("  Saved. Approve this project server before connecting.".into());
            }
            out.push("  /skills show <id> · /skill <id> · /mcp connect <id>".into());
        }
        Panel::Help => out.extend(help(app, width)),
        Panel::None => return out,
    }
    // A panel's keys belong to the panel. The screen carries no permanent
    // shortcut row — a row of hints that is always there is an advertisement —
    // so a panel that needs keys explained says so itself, and only while it is
    // open.
    out.push(String::new());
    out.push(format!("  {FAINT}{}{}", hint(app), ansi::RESET));
    out
}

fn hint(app: &App) -> String {
    match app.panel {
        Panel::Settings => super::settings::hint(app),
        Panel::Undo | Panel::Redo => String::new(),
        Panel::Logs => {
            "↑↓/wheel choose · Enter details · PgUp/PgDn read open details · /logs all|warn|error · Esc close"
                .into()
        }
        // A panel with nothing to choose takes no keys, so the composer is still
        // live underneath it — which is what a person does next after reading
        // one. See [`key`].
        Panel::Context => "pgup/pgdn or the wheel to scroll · esc close".into(),
        Panel::Tasks | Panel::Help | Panel::Capabilities | Panel::None => {
            "esc close · or just carry on typing".into()
        }
    }
}

/// One setting, as a person reads it. Shared with the printed report: a switch
/// that says `on` in the panel and `true` on the command line is two vocabularies
/// for one value.
pub fn value_of(settings: &Value, key: &str) -> String {
    if key == "effort" {
        if settings.get("thinking").and_then(Value::as_bool) == Some(false) {
            return "off".to_string();
        }
        if let Some(eff) = settings
            .get("effort")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty() && *s != "off")
        {
            return eff.to_string();
        }
        if settings.get("thinking").and_then(Value::as_bool) == Some(true) {
            return "on".to_string();
        }
        return "off".to_string();
    }
    match settings.get(key) {
        // Not "empty" and not "broken": the endpoint's own default is in force,
        // and saying so is the difference between a panel that looks unloaded
        // and one that is telling the truth.
        None | Some(Value::Null) => "provider default".to_string(),
        Some(Value::Bool(b)) => if *b { "on" } else { "off" }.to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        Some(v) => v.to_string().trim_matches('"').to_string(),
    }
}

/// The heading every panel opens with.
///
/// One shape, one place. The panels used to be a title, a rule of one arbitrary
/// length, and then whatever each function felt like — which is why they read as
/// four unrelated screens rather than four views of one program. `note` is the
/// line that says what the panel is *for*, so a person who opened it by accident
/// can tell in one glance.
///
/// A panel is deliberately **not** held to the transcript's "content is neutral,
/// furniture is themed" rule. A panel is a surface the reader navigates — a
/// status, a plan, a list of keys — and the theme is what tells them the program
/// has a shape of its own. The owner's call, after the panels were made neutral
/// and the plan read like a wall of grey.
fn title(text: &str, note: &str, width: usize) -> Vec<String> {
    let rule = width.saturating_sub(4).min(72);
    let mut out = vec![
        String::new(),
        format!("  {ACCENT}{}{text}{}", ansi::BOLD, ansi::RESET),
    ];
    if !note.is_empty() {
        out.extend(paragraph(note, FAINT, 2, width));
    }
    out.push(format!("  {RULE}{}{}", "─".repeat(rule), ansi::RESET));
    out.push(String::new());
    out
}

/// A labelled row, used by every panel so the columns line up across all of
/// them.
fn row(label: &str, value: String) -> String {
    format!("  {FAINT}{label:<11}{} {value}", ansi::RESET)
}

/// A section title with a rule running out to the edge.
///
/// One implementation for every panel in the program — the settings screen, the
/// plan, the help, and the column beside the transcript. They were drawn two
/// different ways, which is one way too many for the same idea, and the dimmer
/// of the two is what made the side panel read as a smudge.
pub fn heading(title: &str, width: usize) -> String {
    let rule = width.saturating_sub(ansi::width(title) + 2);
    format!(
        "{ACCENT}{}{title}{} {RULE}{}{}",
        ansi::BOLD,
        ansi::RESET,
        "─".repeat(rule),
        ansi::RESET
    )
}

/// One FILE CHANGE row, shared by the side panel and the theme preview: a
/// bullet, the path, and its counts aligned against a fixed right edge — one
/// line per path, the way an edit note reads in the transcript. `path` and
/// `counts` arrive already truncated and painted by the caller, which owns the
/// geometry through [`file_change_path_w`] and [`file_change_counts_w`], so
/// every `+` starts at the same place — a ragged row reads as a layout bug,
/// not as a table. The bullet is what keeps these rows from reading as plan
/// items: the owner's call after the two lists ran together on screen.
pub fn file_change_row(path_w: usize, counts_w: usize, path: &str, counts: &str) -> String {
    format!(
        "{MUTED}●{RESET} {} {}",
        ansi::pad(path, path_w),
        ansi::pad(counts, counts_w)
    )
}

/// The path column a FILE CHANGE table gives itself: the panel's width minus
/// the counts column, the bullet and the gap after it. One formula for the
/// panel and the theme preview alike, so a path truncated for one is the path
/// the other would have drawn.
pub fn file_change_path_w(width: usize, counts_w: usize) -> usize {
    width.saturating_sub(counts_w + 3).max(1)
}

/// The counts column a FILE CHANGE table gives itself: wide enough for the
/// widest counts it holds. Fixed at the table level rather than per row, so
/// the numbers line up vertically instead of drifting.
pub fn file_change_counts_w(counts: &[String]) -> usize {
    counts.iter().map(|c| ansi::width(c)).max().unwrap_or(0)
}

// ─── The plan, undo history, and help ────────────────────────────────────────

fn tasks(app: &App, width: usize) -> Vec<String> {
    let mut out = title(
        "PLAN",
        "what the agent said it would do, and how far it has got",
        width,
    );
    if app.tasks.is_empty() {
        out.push(format!(
            "  {MUTED}The agent has not written a plan for this session.{}",
            ansi::RESET
        ));
        return out;
    }
    let done = app.tasks.iter().filter(|t| t["status"] == "done").count();
    out.push(row(
        "progress",
        format!("{TEXT}{done} of {} done{}", app.tasks.len(), ansi::RESET),
    ));
    out.push(String::new());
    for task in &app.tasks {
        for line in task_lines(task, width.saturating_sub(2)) {
            out.push(format!("  {line}"));
        }
    }
    out
}

/// One item of the plan: what it is, and whether it is done.
///
/// If the text exceeds the available column width, it wraps across multiple
/// lines with continuation rows indented by two spaces to align beneath the
/// bullet glyph.
pub fn task_lines(task: &Value, width: usize) -> Vec<String> {
    let status = task["status"].as_str().unwrap_or("todo");
    let (glyph, colour, text_style) = match status {
        "done" => ("●", TEXT, FAINT),
        "doing" => ("◐", TEXT, TEXT),
        _ => ("○", MUTED, MUTED),
    };
    let text = task["text"].as_str().unwrap_or("").trim();
    if text.is_empty() {
        return vec![format!("{colour}{glyph}{}", ansi::RESET)];
    }
    let text_w = width.saturating_sub(2).max(1);
    let wrapped = ansi::wrap(text, text_w);
    let mut out = Vec::with_capacity(wrapped.len());
    for (i, line) in wrapped.into_iter().enumerate() {
        let trimmed = line.trim_end();
        if i == 0 {
            out.push(format!(
                "{colour}{glyph}{} {text_style}{trimmed}{}",
                ansi::RESET,
                ansi::RESET
            ));
        } else {
            out.push(format!("  {text_style}{trimmed}{}", ansi::RESET));
        }
    }
    out
}

/// Body wrapping always happens inside its own column. The continuation never
/// steals the turn/time columns, and every caller uses display columns, not bytes.
pub(super) fn paragraph(text: &str, ink: ansi::Ink, indent: usize, width: usize) -> Vec<String> {
    let room = width.saturating_sub(indent + 2).max(1);
    let pad = " ".repeat(indent);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            rows.push(String::new());
        } else {
            rows.extend(
                ansi::wrap(&ansi::plain(line), room)
                    .into_iter()
                    .map(|line| format!("{pad}{ink}{line}{RESET}")),
            );
        }
    }
    rows
}

fn separator(width: usize) -> String {
    format!("    {RULE}{}{RESET}", "─".repeat(width.saturating_sub(6)))
}

fn help(app: &App, width: usize) -> Vec<String> {
    let mut out = title("KEYS", "everything this screen answers to", width);
    for (key, what) in [
        ("ctrl+o", "the session panel: context, cache, spend"),
        ("ctrl+r", "open every folded step and every thought"),
        ("ctrl+g", "settings — model, sampling, providers"),
        ("ctrl+t", "the agent's plan"),
        (
            "/undo",
            "put a file back the way it was before this session",
        ),
        ("/redo", "put back what an undo of a file replaced"),
        ("ctrl+l", "repaint the whole screen"),
        ("alt+t", "cycle thinking: off → provider effort levels"),
        ("tab", "permission mode: Ask → Auto → Full"),
        (
            "ctrl+c",
            "twice to leave · never stops a running turn on first press",
        ),
        ("esc", "twice to stop AI · close a panel when idle"),
        ("↑ ↓", "walk the command list, or a panel's rows"),
        ("← → enter", "answer a permission question"),
        (
            "pgup/pgdn",
            "composer history; ↑/↓ move rows then history at ends",
        ),
        ("ctrl+↓", "jump to the latest row of the chat"),
        ("ctrl/alt+wheel", "scroll the chat 3× as far"),
    ] {
        out.push(format!(
            "  {ACCENT}{key:<14}{} {MUTED}{what}{}",
            ansi::RESET,
            ansi::RESET
        ));
    }
    out.push(String::new());
    out.extend(title(
        "TEXT EDITING",
        "one canonical key per action · temporary history is field-local",
        width,
    ));
    for (key, _, action) in super::editing::BINDINGS {
        out.push(format!(
            "  {ACCENT}{key:<18}{} {MUTED}{}{}",
            ansi::RESET,
            action.description(),
            ansi::RESET
        ));
    }
    out.push(String::new());
    out.extend(title(
        "COMMANDS",
        "type / in the composer and the list comes to you",
        width,
    ));
    // The same table the palette searches and `slash` dispatches from. See
    // [`super::keys::COMMANDS`].
    for (verb, args, what) in super::keys::COMMANDS {
        let name = if args.is_empty() {
            format!("/{verb}")
        } else {
            format!("/{verb} {args}")
        };
        let name = format!("{name:<32}");
        out.push(format!(
            "  {ACCENT}{name}{} {MUTED}{what}{}",
            ansi::RESET,
            ansi::RESET
        ));
    }

    // Where the sandbox report went when the opening screen stopped carrying it.
    // It is still said in full — an engine that claims to be confined when it is
    // not is worse than one that never claimed it — but it is said here, to
    // somebody who came looking, rather than to somebody who has just opened the
    // program to type.
    out.push(String::new());
    out.extend(title(
        "THIS MACHINE",
        "what is enforcing, and what can get past it",
        width,
    ));
    out.push(row(
        "session",
        format!(
            "{MUTED}{} · {}{}",
            app.session
                .map_or_else(|| "Draft".into(), |id| format!("#{id}")),
            app.project,
            ansi::RESET
        ),
    ));
    out.push(row(
        "sandbox",
        if app.sandbox_ok {
            format!("{OK}{}{}", app.sandbox, ansi::RESET)
        } else {
            format!("{WARN}off — {}{}", app.sandbox, ansi::RESET)
        },
    ));
    if app.escapes.is_empty() {
        out.push(row(
            "ways out",
            format!("{MUTED}none found on this machine{}", ansi::RESET),
        ));
    }
    for escape in &app.escapes {
        for (n, line) in ansi::wrap(escape, width.saturating_sub(18))
            .into_iter()
            .enumerate()
        {
            let label = if n == 0 { "ways out" } else { "" };
            out.push(row(label, format!("{WARN}{line}{}", ansi::RESET)));
        }
    }
    out
}

// ─── Keys ─────────────────────────────────────────────────────────────────────

/// Give a panel the key, and say whether it wanted it.
///
/// The lines the log panel shows: every line at or above the chosen level.
fn shown_logs(app: &App) -> Vec<&runeengine::proto::LogEntry> {
    app.logs
        .iter()
        .filter(|entry| entry.level >= app.log_floor)
        .collect()
}

/// `/context`: what the model holds beside the conversation — the system prompt,
/// the tools declared, every message Rune wrote into the session, the summary of
/// a compressed past and the values in `<status>` — exactly as the engine says
/// it was sent, so a person can read what the model reads.
fn context(app: &App, width: usize) -> Vec<String> {
    context_document(app, width).sink.lines
}

fn context_document(app: &App, width: usize) -> super::panel_view::Document {
    use super::panel_view::Document;
    let mut doc = Document::reader();
    doc.append(
        "title",
        title(
            "CONTEXT",
            "what the model holds besides the conversation on screen",
            width,
        ),
    );
    let view = &app.context_view;
    if view.is_null() {
        doc.append("empty", paragraph("Reading…", MUTED, 2, width));
        return doc;
    }
    let empty = Vec::new();
    let tools = view["tools"].as_array().unwrap_or(&empty);
    let messages = view["context"].as_array().unwrap_or(&empty);
    doc.append(
        "overview",
        paragraph(
            &format!(
                "{} tools · {} injected messages · read-only",
                tools.len(),
                messages.len()
            ),
            MUTED,
            2,
            width,
        ),
    );
    let system = view["system"].as_str().unwrap_or("");
    context_section(
        &mut doc,
        "system",
        &format!("SYSTEM PROMPT · {} bytes", system.len()),
        width,
    );
    context_text(&mut doc, "system", system, width);

    context_section(
        &mut doc,
        "tools",
        &format!("TOOLS · {} declared", tools.len()),
        width,
    );
    for (i, tool) in tools.iter().enumerate() {
        let name = tool["name"].as_str().unwrap_or("?");
        let key = format!("tool:{name}");
        if i > 0 {
            doc.append(format!("{key}:rule"), vec![separator(width)]);
        }
        doc.append(format!("{key}:name"), paragraph(name, ACCENT, 4, width));
        let required: Vec<&str> = tool["parameters"]["required"]
            .as_array()
            .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        if let Some(props) = tool["parameters"]["properties"].as_object() {
            let names: Vec<String> = props
                .keys()
                .map(|k| {
                    if required.contains(&k.as_str()) {
                        k.clone()
                    } else {
                        format!("{k}?")
                    }
                })
                .collect();
            doc.append(
                format!("{key}:arguments"),
                paragraph(&format!("arguments: {}", names.join(", ")), MUTED, 6, width),
            );
        }
        context_text(
            &mut doc,
            &key,
            tool["description"].as_str().unwrap_or(""),
            width,
        );
    }

    context_section(
        &mut doc,
        "messages",
        &format!("MESSAGES RUNE WROTE INTO THIS SESSION · {}", messages.len()),
        width,
    );
    if messages.is_empty() {
        doc.append(
            "messages:empty",
            paragraph(
                "None yet: the first message brings the opening context.",
                MUTED,
                4,
                width,
            ),
        );
    }
    for (i, row) in messages.iter().enumerate() {
        let key = format!("message:{}", row["id"].as_i64().unwrap_or(i as i64));
        if i > 0 {
            doc.append(format!("{key}:rule"), vec![separator(width)]);
        }
        let kind = row["kind"].as_str().unwrap_or("context");
        let name = row["name"]
            .as_str()
            .map(|n| format!(" · {n}"))
            .unwrap_or_default();
        doc.append(
            format!("{key}:kind"),
            paragraph(&format!("{kind}{name}"), ACCENT, 4, width),
        );
        let time = row["at"]
            .as_i64()
            .map(super::term::epoch_hms)
            .unwrap_or_default();
        doc.append(
            format!("{key}:metadata"),
            paragraph(
                &format!(
                    "turn {} · {time} · {} bytes",
                    row["turn"].as_i64().unwrap_or(0),
                    row["text"].as_str().unwrap_or("").len()
                ),
                MUTED,
                6,
                width,
            ),
        );
        context_text(&mut doc, &key, row["text"].as_str().unwrap_or(""), width);
    }
    if let Some(summary) = view["summary"].as_str().filter(|s| !s.is_empty()) {
        context_section(
            &mut doc,
            "summary",
            "SUMMARY OF THE EARLIER CONVERSATION",
            width,
        );
        context_text(&mut doc, "summary", summary, width);
    }
    if let Some(snapshot) = view["summary_context"].as_str().filter(|s| !s.is_empty()) {
        context_section(
            &mut doc,
            "summary_context",
            "CONTEXT AT THE LAST COMPACTION",
            width,
        );
        context_text(&mut doc, "summary_context", snapshot, width);
    }
    context_section(&mut doc, "status", "STATUS NOW", width);
    let status = view["status"].as_array().unwrap_or(&empty);
    let name_w = status
        .iter()
        .filter_map(|s| s["name"].as_str())
        .map(ansi::width)
        .max()
        .unwrap_or(0)
        .min(width.saturating_sub(16));
    for entry in status {
        let name = ansi::plain(entry["name"].as_str().unwrap_or(""));
        let value = ansi::plain(entry["value"].as_str().unwrap_or(""));
        let indent = 4 + name_w + 2;
        let mut rows = Vec::new();
        for (i, part) in ansi::wrap(&value, width.saturating_sub(indent + 2).max(1))
            .iter()
            .enumerate()
        {
            let head = if i == 0 {
                format!(
                    "    {MUTED}{}{RESET}  ",
                    ansi::pad(&ansi::truncate(&name, name_w), name_w)
                )
            } else {
                " ".repeat(indent)
            };
            rows.push(format!("{head}{TEXT}{part}{RESET}"));
        }
        doc.append(format!("status:{name}"), rows);
    }
    doc
}

fn context_section(doc: &mut super::panel_view::Document, key: &str, label: &str, width: usize) {
    let inner = width.saturating_sub(4);
    doc.append(
        format!("section:{key}"),
        vec![
            String::new(),
            format!("  {}", heading(&ansi::truncate(label, inner), inner)),
            String::new(),
        ],
    );
}

fn context_text(doc: &mut super::panel_view::Document, key: &str, text: &str, width: usize) {
    let room = width.saturating_sub(6).max(1);
    for (i, line) in text.split('\n').enumerate() {
        let parts = if line.is_empty() {
            vec![String::new()]
        } else {
            ansi::wrap(&ansi::plain(line), room)
        };
        doc.append(
            format!("{key}:line:{i}"),
            parts
                .into_iter()
                .map(|part| format!("    {TEXT}{part}{RESET}"))
                .collect(),
        );
    }
}

/// `/logs`: what happened behind the conversation, oldest first so the newest
/// sits where a reader's eye already is — at the bottom, by the composer. Each
/// line is its time to the second, its level, what it is about, and one
/// sentence; `enter` opens what the sentence was built from.
fn logs(app: &App, width: usize) -> Vec<String> {
    logs_document(app, width).sink.lines
}

fn logs_document(app: &App, width: usize) -> super::panel_view::Document {
    use runeengine::proto::LogLevel;
    let mut doc = super::panel_view::Document::reader();
    doc.centre = true;
    let mut out = title(
        "LOGS",
        "what happened behind the conversation, newest last",
        width,
    );
    let count = |level: LogLevel| app.logs.iter().filter(|e| e.level == level).count();
    let showing = match app.log_floor {
        LogLevel::Info => "showing everything",
        LogLevel::Warn => "showing warnings and errors",
        LogLevel::Error => "showing errors",
    };
    out.push(row(
        "logged",
        format!(
            "{TEXT}{} info · {} {} · {} {}{}  {FAINT}{showing}{}",
            count(LogLevel::Info),
            count(LogLevel::Warn),
            if count(LogLevel::Warn) == 1 {
                "warning"
            } else {
                "warnings"
            },
            count(LogLevel::Error),
            if count(LogLevel::Error) == 1 {
                "error"
            } else {
                "errors"
            },
            ansi::RESET,
            ansi::RESET
        ),
    ));
    out.push(String::new());
    let shown = shown_logs(app);
    doc.append("header", std::mem::take(&mut out));
    if shown.is_empty() {
        out.push(format!(
            "  {MUTED}Nothing logged in this session yet.{}",
            ansi::RESET
        ));
        doc.append("empty", out);
        return doc;
    }
    let selected = app.panel_row.min(shown.len() - 1);
    // cursor, time, level and source take this many columns before the text.
    const SOURCE: usize = 10;
    let indent = 2 + 1 + 1 + 8 + 2 + 5 + 2 + SOURCE + 1;
    let text_width = width.saturating_sub(indent).max(20);
    for (i, entry) in shown.iter().enumerate() {
        let title_row = usize::from(i > 0);
        if i > 0 {
            let rule_w = width.saturating_sub(6).max(10);
            out.push(format!("    {RULE}{}{RESET}", "─".repeat(rule_w)));
        }
        let cursor = if i == selected {
            format!("{ACCENT}›{}", ansi::RESET)
        } else {
            " ".into()
        };
        let (level, ink) = match entry.level {
            LogLevel::Info => ("info", MUTED),
            LogLevel::Warn => ("warn", WARN),
            LogLevel::Error => ("error", super::ansi::REMOVED),
        };
        let source: String = entry.source.chars().take(SOURCE).collect();
        let head = format!(
            "  {cursor} {FAINT}{}{}  {ink}{level:<5}{}  {MUTED}{source:<SOURCE$}{} ",
            super::term::epoch_hms(entry.at),
            ansi::RESET,
            ansi::RESET,
            ansi::RESET
        );
        for (k, line) in ansi::wrap(&ansi::plain(&entry.message), text_width)
            .iter()
            .enumerate()
        {
            if k == 0 {
                out.push(format!("{head}{TEXT}{line}{}", ansi::RESET));
            } else {
                out.push(format!("{}{TEXT}{line}{}", " ".repeat(indent), ansi::RESET));
            }
        }
        if app.log_open == Some(i) {
            let pad = " ".repeat(indent);
            let mut facts = vec![format!("code {}", entry.code)];
            if let Some(turn) = entry.turn {
                facts.push(format!("turn {turn}"));
            }
            out.push(format!("{pad}{FAINT}{}{}", facts.join(" · "), ansi::RESET));
            if let Some(detail) = &entry.detail {
                // The text a line was built from — the context the model was
                // given, a status update — is shown as the text it is, line by
                // line; only the numbers beside it are shown as data.
                if let Some(text) = detail["text"].as_str() {
                    for line in text.lines() {
                        for wrapped in ansi::wrap(&ansi::plain(line), text_width) {
                            out.push(format!("{pad}{FAINT}{wrapped}{}", ansi::RESET));
                        }
                    }
                }
                let mut numbers = detail.clone();
                if let Some(map) = numbers.as_object_mut() {
                    map.remove("text");
                }
                if numbers.as_object().is_none_or(|map| !map.is_empty()) {
                    let pretty = serde_json::to_string_pretty(&numbers).unwrap_or_default();
                    for line in pretty.lines() {
                        for wrapped in ansi::wrap(line, text_width) {
                            out.push(format!("{pad}{FAINT}{wrapped}{}", ansi::RESET));
                        }
                    }
                }
            }
        }
        let range = doc.append_at(
            format!("log:{}:{}:{i}", entry.at, entry.code),
            std::mem::take(&mut out),
            |_| super::draw::Hit::Log(i),
        );
        if i == selected {
            let start = range.start + title_row;
            doc.focus = Some(start..start + 1);
        }
    }
    doc
}

/// # A panel that takes every key is a locked keyboard
///
/// This used to consume the lot. Opening `/help` therefore froze the composer:
/// every letter went to a screen with nothing to type into, and the only way
/// back to the cursor was `esc` — which is not something a person guesses while
/// their sentence is going nowhere. A panel now takes **only the keys it
/// actually uses**, and everything else falls through to the composer, so `/help`
/// and the plan are read *over* a live question rather than instead of one.
///
/// The two panels that do have rows keep their keys: `↑↓` where there is
/// something to choose, and the settings letters, which are the whole of that
/// screen's interface.
pub fn wheel(app: &mut App, up: bool, multiplier: usize, d: &mut Driver) {
    if app.panel == Panel::Logs
        || (matches!(app.panel, Panel::Undo | Panel::Redo) && app.undo_diff.is_none())
    {
        for _ in 0..multiplier {
            key(app, if up { &Key::Up } else { &Key::Down }, d);
        }
    } else {
        let rows = super::runtime_configuration().number("ui.scroll_lines") as usize;
        app.panel_view
            .scroll_by(up, rows.saturating_mul(multiplier));
    }
}

pub fn click_log(app: &mut App, index: usize) {
    if app.panel == Panel::Logs {
        app.panel_row = index.min(shown_logs(app).len().saturating_sub(1));
        app.panel_view.reveal();
    }
}

pub fn key(app: &mut App, key: &Key, d: &mut Driver) -> bool {
    if app.panel == Panel::Settings {
        return super::settings::key(app, key, d);
    }
    if matches!(app.panel, Panel::Undo | Panel::Redo) {
        return super::undo::key(app, key, d);
    }
    let rows = if app.panel == Panel::Logs {
        shown_logs(app).len()
    } else {
        0
    };
    if app.panel == Panel::Logs {
        if app.log_open.is_some() && matches!(key, Key::PageUp | Key::PageDown) {
            let page = app.panel_view.page();
            app.panel_view.scroll_by(*key == Key::PageUp, page);
            return true;
        }
        let at = app.panel_row.min(rows.saturating_sub(1));
        let to = match key {
            Key::Up => Some(at.saturating_sub(1)),
            Key::Down => Some((at + 1).min(rows.saturating_sub(1))),
            Key::PageUp => Some(at.saturating_sub(5)),
            Key::PageDown => Some((at + 5).min(rows.saturating_sub(1))),
            Key::Home => Some(0),
            Key::End => Some(rows.saturating_sub(1)),
            Key::Enter => {
                if rows > 0 {
                    app.log_open = if app.log_open == Some(at) {
                        None
                    } else {
                        Some(at)
                    };
                }
                app.panel_view.reveal();
                return true;
            }
            _ => None,
        };
        if let Some(to) = to {
            app.panel_row = to;
            app.panel_view.reveal();
            return true;
        }
    }
    let reader = matches!(
        app.panel,
        Panel::Context | Panel::Help | Panel::Tasks | Panel::Capabilities
    );
    match key {
        Key::Enter
            if reader
                && (app.questions.is_some()
                    || app.mcp_input.is_some()
                    || app.pending.is_some()) =>
        {
            true
        }
        Key::Up if reader => {
            app.panel_view.scroll_by(true, 1);
            true
        }
        Key::Down if reader => {
            app.panel_view.scroll_by(false, 1);
            true
        }
        Key::PageUp if reader => {
            let rows = app.panel_view.page();
            app.panel_view.scroll_by(true, rows);
            true
        }
        Key::PageDown if reader => {
            let rows = app.panel_view.page();
            app.panel_view.scroll_by(false, rows);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::ansi::FAINT;
    use crate::ui::App;
    use serde_json::json;

    fn text(lines: Vec<String>) -> String {
        lines
            .iter()
            .map(|l| ansi::plain(l))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn configured() -> App {
        let mut app = App::for_test();
        app.settings = json!({
            "settings": { "provider": "mimo", "model": "mimo-v2.5-pro", "thinking": true },
            "context": 131072,
            "max_output_ceiling": 131072,
            "knobs": [
                { "key": "thinking", "label": "Thinking", "kind": "bool", "help": "reason first" },
                { "key": "temperature", "label": "Temperature", "kind": "float",
                  "min": 0.0, "max": 1.5, "step": 0.05, "help": "randomness" },
                { "key": "effort", "label": "Effort", "kind": "choice",
                  "choices": ["low", "medium", "high"], "help": "how much reasoning" },
                { "key": "max_output", "label": "Max output", "kind": "tokens",
                  "help": "ceiling on one reply" }
            ],
            "suppressed": [
                { "key": "temperature", "reason": "ignored while thinking is on",
                  "permanent": false },
                { "key": "effort", "reason": "this provider has no effort level",
                  "permanent": true }
            ]
        });
        app.providers = json!({
            "providers": [{ "name": "mimo", "needs_key": true, "has_key": true, "default": true }],
            "web_search": false
        });
        app.panel = Panel::Settings;
        app
    }

    /// A panel is a surface the reader *navigates* — a plan, a status, a list of
    /// keys — not the conversation, and the owner has it carry the theme. This
    /// test exists so the neutral-panel experiment does not come back a second
    /// time: the transcript's "content is neutral" rule stops at the panel door.
    #[test]
    fn a_panel_carries_the_theme_even_though_its_words_are_a_heading() {
        assert!(
            heading("PLAN 1/2", 40).contains(&ACCENT.to_string()),
            "the plan heading lost the theme"
        );
        let panel_title = title("PLAN", "what the agent said it would do", 40).join("\n");
        assert!(
            panel_title.contains(&ACCENT.to_string()) && panel_title.contains(&FAINT.to_string()),
            "the panel title and subtitle lost the theme"
        );
        let done = task_lines(
            &json!({ "text": "wire the permission broker", "status": "done" }),
            40,
        )
        .join("\n");
        assert!(done.contains("●"), "a done item is filled: {done}");
        assert!(
            done.contains(&FAINT.to_string()),
            "a done plan item lost the theme: {done}"
        );
        let todo =
            task_lines(&json!({ "text": "write the docs", "status": "todo" }), 40).join("\n");
        assert!(todo.contains("○"), "a pending item is hollow: {todo}");
    }

    #[test]
    fn every_panel_opens_the_same_way() {
        let mut app = configured();
        app.tasks = vec![json!({ "text": "read the code", "status": "done" })];
        app.file_changes = vec![json!({ "path": "a.rs", "added": 1, "removed": 0 })];
        for (panel, heading) in [
            (Panel::Settings, "SETTINGS"),
            (Panel::Tasks, "PLAN"),
            (Panel::Undo, "UNDO"),
            (Panel::Help, "KEYS"),
        ] {
            app.panel = panel;
            let lines = render(&app, 80).lines;
            let out = text(lines.clone());
            assert!(out.contains(heading), "{heading} is missing:\n{out}");
            assert!(
                lines.iter().any(|l| ansi::plain(l).contains('─')),
                "{heading} has no rule under its title"
            );
        }
    }

    /// The fault the owner hit: `/help` swallowed every keystroke, so the
    /// composer went dead and the only way out was `esc` — which is not what a
    /// person reaches for while their sentence is going nowhere.
    #[test]
    fn a_panel_with_nothing_to_choose_leaves_the_keyboard_to_the_composer() {
        let mut d = crate::ui::test_driver("panelkeys");
        let typed = [Key::Char('h'), Key::Char('i'), Key::Enter, Key::Backspace];
        for panel in [Panel::Help, Panel::Tasks] {
            let mut app = App::for_test();
            app.panel = panel;
            for pressed in &typed {
                assert!(
                    !key(&mut app, pressed, &mut d),
                    "{pressed:?} was eaten by a panel with nothing to press it on"
                );
            }
            for pressed in [Key::Up, Key::Down, Key::PageUp, Key::PageDown] {
                assert!(key(&mut app, &pressed, &mut d), "reader lost {pressed:?}");
            }
        }

        // …while the two that do have rows still keep the keys they need. The
        // Settings owns its whole screen; unused letters must not leak into
        // the hidden composer. Models uses letters for visible search.
        let mut app = configured();
        app.panel = Panel::Settings;
        assert!(
            key(&mut app, &Key::Down, &mut d),
            "the settings screen gave up its own arrows"
        );
        for letter in ['m', 'p', 'a', 'w', 'z'] {
            assert!(
                key(&mut app, &Key::Char(letter), &mut d),
                "Settings let `{letter}` leak into the hidden composer"
            );
        }
    }

    #[test]
    fn the_plan_panel_says_how_far_along_it_is() {
        let mut app = configured();
        app.panel = Panel::Tasks;
        app.tasks = vec![
            json!({ "text": "read the code", "status": "done" }),
            json!({ "text": "fix it", "status": "doing" }),
        ];
        let out = text(render(&app, 80).lines);
        assert!(out.contains("1 of 2 done"), "{out}");
        assert!(out.contains("● read the code"));
    }

    #[test]
    fn no_panel_line_is_wider_than_the_space_it_was_given() {
        let mut app = configured();
        app.tasks = vec![json!({ "text": "x".repeat(200), "status": "todo" })];
        app.file_changes = vec![json!({ "path": "a.rs", "added": 1, "removed": 0 })];
        for panel in [Panel::Settings, Panel::Tasks, Panel::Undo, Panel::Help] {
            app.panel = panel;
            for width in [40usize, 80, 120] {
                for line in render(&app, width).lines {
                    // The composer truncates every content line to the panel's
                    // width; this asserts the panel does not depend on that for
                    // anything but a pathological string.
                    assert!(ansi::width(&line) <= width.max(80) + 200);
                }
            }
        }
    }

    #[test]
    fn the_plan_says_so_plainly_when_there_is_none() {
        let app = App::for_test();
        assert!(tasks(&app, 80)
            .join("\n")
            .contains("has not written a plan"));
    }

    #[test]
    fn context_separates_items_and_wraps_status_values_in_their_column() {
        let mut app = App::for_test();
        app.panel = Panel::Context;
        app.context_view = json!({
            "system": "line one\n\nline three",
            "tools": [{"name": "first", "description": "long description ".repeat(10)}, {"name": "second", "description": "other"}],
            "context": [{"id":1,"kind":"session","turn":1,"text":"row one\n\nrow three"}, {"id":2,"kind":"status","turn":2,"text":"update"}],
            "status": [{"name":"workspace","value":"/very-long-directory/".repeat(10)}],
            "summary_context": "compacted context"
        });
        for width in [40, 56, 80, 120] {
            let lines = document(&app, width).sink.lines;
            assert!(
                lines.iter().all(|line| ansi::width(line) <= width),
                "width {width}: {lines:?}"
            );
            let plain: Vec<_> = lines.iter().map(|line| ansi::plain(line)).collect();
            assert!(
                plain
                    .iter()
                    .filter(|line| line.starts_with("    ─"))
                    .count()
                    >= 2
            );
            assert!(
                plain.iter().any(|line| line == "    "),
                "one blank source line is preserved once"
            );
            assert!(plain.iter().all(|line| !line.contains('│')));
            assert!(plain.iter().any(|line| line == "    line one"));
            assert!(plain.iter().any(|line| line.contains("compacted context")));
            let first = plain
                .iter()
                .position(|line| line.contains("workspace"))
                .unwrap();
            let column = plain[first].find('/').unwrap();
            assert!(plain[first + 1].starts_with(&" ".repeat(column)));
        }
    }

    #[test]
    fn every_shortcut_in_the_help_is_one_a_terminal_can_actually_send() {
        let text = help(&App::for_test(), 80).join("\n");
        assert!(!text.contains("ctrl+,"), "a terminal cannot send that");
        assert!(!text.contains("ctrl+s"), "ctrl+s is XOFF and stays unbound");
        assert!(text.contains("ctrl+o"));
    }

    #[test]
    fn task_lines_wraps_long_todos_across_multiple_indented_lines() {
        let task = json!({
            "status": "todo",
            "text": "Patch mod.rs: hapus blok fallback lintas sesi (SessionList) + perbaiki komentar"
        });
        // With width = 30, text_w = 28. The long text must wrap into multiple lines without truncation.
        let lines = task_lines(&task, 30);
        assert!(
            lines.len() > 1,
            "must wrap to multiple lines, got {:?}",
            lines
        );
        let plain_first = ansi::plain(&lines[0]);
        let plain_second = ansi::plain(&lines[1]);
        assert!(
            plain_first.starts_with("○ "),
            "first line starts with bullet glyph"
        );
        assert!(
            plain_second.starts_with("  "),
            "continuation line is indented with two spaces"
        );
        for (i, line) in lines.iter().enumerate() {
            assert!(
                ansi::width(&ansi::plain(line)) <= 30,
                "line {i} must fit in column width (30): {:?}",
                line
            );
        }
    }

    fn entry(
        at: i64,
        level: runeengine::proto::LogLevel,
        source: &str,
        message: &str,
    ) -> runeengine::proto::LogEntry {
        runeengine::proto::LogEntry {
            at,
            level,
            source: source.into(),
            code: "c".into(),
            message: message.into(),
            turn: None,
            detail: Some(serde_json::json!({"tokens": 42})),
        }
    }

    /// `/logs` lists what happened behind the conversation: each line its time
    /// to the second, its level, what it is about and its sentence; the chosen
    /// line is marked with a line chevron, never a filled one, and its detail
    /// opens under it.
    #[test]
    fn the_log_panel_lists_time_level_source_and_message() {
        use runeengine::proto::LogLevel;
        let mut app = configured();
        app.logs = vec![
            entry(
                0,
                LogLevel::Info,
                "context",
                "Opening context sent: 1.2 KB.",
            ),
            entry(
                1_000,
                LogLevel::Warn,
                "title",
                "No session title was generated.",
            ),
        ];
        app.panel = Panel::Logs;
        app.panel_row = usize::MAX;
        let out = text(render(&app, 100).lines);
        assert!(out.contains("LOGS"), "{out}");
        assert!(out.contains("1 info · 1 warning · 0 errors"), "{out}");
        let warn = out
            .lines()
            .find(|l| l.contains("No session title"))
            .expect("the warning is listed");
        assert!(warn.contains("warn") && warn.contains("title"), "{warn}");
        assert!(
            warn.split_whitespace()
                .any(|w| w.len() == 8 && w.matches(':').count() == 2),
            "the time is shown to the second: {warn}"
        );
        assert!(
            warn.contains('›'),
            "the newest line is the chosen one: {warn}"
        );
        for filled in ['▸', '▶', '►', '▾', '▼'] {
            assert!(!out.contains(filled), "a filled chevron was drawn: {out}");
        }
        // Rows have a divider between them with margins
        assert!(out.lines().any(|l| l.starts_with("    ─")));
        app.log_open = Some(1);
        let open = text(render(&app, 100).lines);
        assert!(open.contains("\"tokens\": 42"), "the detail opens: {open}");
    }

    /// A line built from text — the context the model was given — opens to
    /// that text as text, line by line, with only the numbers beside it as data.
    #[test]
    fn a_log_line_built_from_text_opens_to_the_text() {
        use runeengine::proto::LogLevel;
        let mut app = configured();
        let mut line = entry(0, LogLevel::Info, "context", "Opening context sent: 51 B.");
        line.detail = Some(serde_json::json!({
            "bytes": 51,
            "text": "<status>\nmodel: m\n</status>"
        }));
        app.logs = vec![line];
        app.panel = Panel::Logs;
        app.panel_row = usize::MAX;
        app.log_open = Some(0);
        let open = text(render(&app, 100).lines);
        assert!(
            open.contains("<status>") && open.contains("model: m"),
            "{open}"
        );
        assert!(
            !open.contains("\\n"),
            "the text is not shown as an escaped JSON string: {open}"
        );
        assert!(open.contains("\"bytes\": 51"), "{open}");
        assert!(!open.contains("\"text\""), "{open}");
    }

    /// `/context` shows what the model holds: the system prompt, every tool, the
    /// messages Rune wrote into the session with their kind and turn, the summary
    /// of a compressed past, and the status now.
    #[test]
    fn the_context_panel_shows_what_the_model_holds() {
        let mut app = configured();
        app.panel = Panel::Context;
        app.context_view = serde_json::json!({
            "system": "<instruction>\nYou are Rune.\n</instruction>",
            "tools": [{
                "name": "terminal_use",
                "description": "Run a command.",
                "parameters": {"properties": {"action": {}, "command": {}, "path": {}}, "required": ["action", "command"]}
            }],
            "context": [
                {"id": 1, "turn": 1, "at": 0, "kind": "session", "name": null,
                 "text": "<status>\nmodel: m\n</status>"},
                {"id": 9, "turn": 4, "at": 0, "kind": "status", "name": null,
                 "text": "<status_update>\nmode: full (was ask)\n</status_update>"}
            ],
            "summary": "We fixed the footer.",
            "summary_context": "",
            "status": [{"name": "model", "value": "m"}, {"name": "workspace", "value": "/p"}]
        });
        let out = text(render(&app, 100).lines);
        for must in [
            "CONTEXT",
            "SYSTEM PROMPT",
            "You are Rune.",
            "TOOLS · 1 declared",
            "terminal_use",
            "arguments: action, command, path?",
            "MESSAGES RUNE WROTE INTO THIS SESSION · 2",
            "session",
            "turn 4",
            "mode: full (was ask)",
            "SUMMARY OF THE EARLIER CONVERSATION",
            "We fixed the footer.",
            "STATUS NOW",
            "workspace",
        ] {
            assert!(out.contains(must), "{must} missing:\n{out}");
        }
        app.context_view = serde_json::Value::Null;
        assert!(text(render(&app, 100).lines).contains("Reading"));
    }

    #[test]
    fn the_log_panel_can_show_only_warnings_and_errors() {
        use runeengine::proto::LogLevel;
        let mut app = configured();
        app.logs = vec![
            entry(0, LogLevel::Info, "usage", "agent request: 10 tokens in."),
            entry(
                1_000,
                LogLevel::Error,
                "compact",
                "Could not summarise history.",
            ),
        ];
        app.panel = Panel::Logs;
        app.log_floor = LogLevel::Warn;
        let out = text(render(&app, 100).lines);
        assert!(out.contains("Could not summarise history."), "{out}");
        assert!(!out.contains("agent request"), "{out}");
        assert!(out.contains("showing warnings and errors"), "{out}");
    }
}
