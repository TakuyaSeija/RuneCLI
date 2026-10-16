//! Format 1 → 2, the release that gave every project one folder (Rune 0.1.5).
//!
//! Three changes, in the order a failure is cheapest to undo: the
//! configuration (one file, saved first), the layout (renames only, the plan
//! written before the first), and the database (its own transaction, a copy
//! taken first).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::Plan;
use crate::error::{Error, Result};
use crate::settings::configuration::FILE;

/// The folders format 1 kept at the top of the home, where format 2 keeps
/// one folder per project.
const OLD_FOLDERS: [&str; 5] = ["sessions", "memory", "objects", "scratch", "Capabilities"];

/// Whether `store` still has format 1's shape.
pub(super) fn written_by_version_1(store: &Path) -> bool {
    OLD_FOLDERS.iter().any(|name| store.join(name).is_dir())
        || store.join("sessions_index.json").exists()
}

/// Whether the configuration in `settings` still uses what format 1
/// accepted and format 2 refuses.
pub(super) fn configured_by_version_1(settings: &Path) -> bool {
    std::fs::read_to_string(settings.join(FILE))
        .ok()
        .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
        .is_some_and(|document| has_retired_settings(&document))
}

/// Upgrade `store` (and the configuration in `settings`) to format 2,
/// writing every change into `plan` before making it.
pub(super) fn apply(store: &Path, settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    configuration(settings, backup, plan)?;
    layout(store, backup, plan)?;
    database(store, backup, plan)
}

/// What [`apply`] would do, checked without doing any of it.
pub(super) fn check(store: &Path, settings: &Path) -> Result<()> {
    if let Ok(text) = std::fs::read_to_string(settings.join(FILE)) {
        super::validate(&config(&text)?)?;
    }
    let sessions = sessions_of(store)?;
    for (dir, project) in old_sessions(store, &sessions) {
        if let Some(project) = project {
            let name = crate::project::find(store, &project).unwrap_or_else(|| {
                store.join("projects").join(crate::project::folder_name(
                    &crate::project::canonical(&project),
                ))
            });
            let target = name.join("sessions").join(dir.file_name().unwrap());
            if target.exists() {
                return Err(Error::invalid(format!(
                    "{} already exists; {} cannot move there",
                    target.display(),
                    dir.display()
                )));
            }
        }
    }
    Ok(())
}

// ── Configuration ─────────────────────────────────────────────────────────────

/// Keys format 1 accepted beside the canonical ones, and where each value goes
/// in format 2. `[inspect_files]` is renamed to `[inspect_file]` first, so the
/// keys format 1 read there are listed under their new table.
const ALIASES: &[(&str, &str)] = &[
    ("keep_turns", "checkpoint.keep_turns"),
    ("command_timeout_ms", "terminal.command_timeout"),
    ("max_command_timeout_ms", "terminal.command_timeout"),
    ("max_command_timeout", "terminal.command_timeout"),
    ("terminal.max_command_timeout", "terminal.command_timeout"),
    ("terminal.max_chars", "terminal.max_output_chars"),
    ("max_result_chars", "terminal.max_output_chars"),
    ("tools.max_result_chars", "terminal.max_output_chars"),
    ("terminal.buffer_limit", "terminal.max_output_size"),
    ("tools.max_output_size", "terminal.max_output_size"),
    (
        "inspect_file.max_text_chars",
        "inspect_file.max_output_chars",
    ),
    ("inspect_file.max_chars", "inspect_file.max_output_chars"),
    ("tools.inspect.max_chars", "inspect_file.max_output_chars"),
    ("tools.inspect.max_file_size", "inspect_file.max_file_size"),
    ("inspect_file.max_binary_size", "inspect_file.max_file_size"),
    (
        "inspect_file.max_total_binary_size",
        "inspect_file.max_file_size",
    ),
    ("find_files.default_matches", "find_files.matches"),
    ("find_files.max_matches", "find_files.matches"),
    ("find_files.default_limit", "find_files.matches"),
    ("find_files.max_limit", "find_files.matches"),
    ("tools.find.matches", "find_files.matches"),
    ("tools.find.default_limit", "find_files.matches"),
    ("tools.find.max_limit", "find_files.matches"),
    ("find_files.max_chars", "find_files.max_output_chars"),
    ("search_text.default_matches", "search_text.matches"),
    ("search_text.max_matches", "search_text.matches"),
    ("search_text.default_limit", "search_text.matches"),
    ("search_text.max_limit", "search_text.matches"),
    ("tools.search.matches", "search_text.matches"),
    ("tools.search.default_limit", "search_text.matches"),
    ("tools.search.max_limit", "search_text.matches"),
    ("tools.search.max_line_chars", "search_text.max_line_chars"),
    ("tools.search.max_file_size", "search_text.max_file_size"),
    ("search_text.max_chars", "search_text.max_output_chars"),
    ("patch_files.max_file_size", "checkpoint.max_file_size"),
    ("tools.patch.max_file_size", "checkpoint.max_file_size"),
    (
        "manage_process.max_chars",
        "manage_process.max_output_chars",
    ),
    ("monitoring_bg.max_chars", "manage_process.max_output_chars"),
    ("web_search.default_results", "web_search.results"),
    ("web_search.max_results", "web_search.results"),
    ("web_search.max_result_chars", "web_search.max_output_chars"),
    (
        "discover_capabilities.max_chars",
        "discover_capabilities.max_output_chars",
    ),
    (
        "read_mcp_resource.max_chars",
        "read_mcp_resource.max_output_chars",
    ),
    (
        "read_mcp_resource.max_binary_size",
        "read_mcp_resource.max_output_chars",
    ),
    ("tasks.max_text_chars", "tasks.max_chars_per_task"),
    (
        "max_tool_requests_per_response",
        "tools.max_actions_per_response",
    ),
    ("compact_at_tokens", "compact.fallback_context_tokens"),
    ("keep_recent_turns", "compact.keep_recent_turns"),
    ("warn_spend_usd", "usage.warn_spend_usd"),
    ("mouse", "ui.mouse"),
    ("theme", "ui.theme"),
    ("sidebar", "ui.sidebar"),
];

/// Switches format 1 read and ignored, now simply gone.
const RETIRED: &[&str] = &[
    "terminal.background_on_timeout",
    "terminal.persisted_output",
    "manage_process.persisted_output",
    "attachments.max_inline_size",
    "attachments.max_payload_size",
];

/// Tables format 1 accepted that format 2 does not have, removed once the
/// keys above moved out of them.
const OLD_TABLES: &[&str] = &[
    "tools.inspect",
    "tools.find",
    "tools.search",
    "tools.patch",
    "monitoring_bg",
    "patch_files",
    "patch_file",
    "inspect_files",
];

/// The file tools' names in format 1.
const TOOL_NAMES: &[(&str, &str)] = &[
    ("inspect_files", "inspect_file"),
    ("create_files", "create_file"),
    ("patch_files", "patch_file"),
];

fn has_retired_settings(document: &toml_edit::DocumentMut) -> bool {
    ALIASES.iter().any(|(old, _)| item(document, old).is_some())
        || RETIRED.iter().any(|key| item(document, key).is_some())
        || OLD_TABLES.iter().any(|key| item(document, key).is_some())
        || excluded_tools(document)
            .iter()
            .any(|name| TOOL_NAMES.iter().any(|(old, _)| name == old))
}

/// `rune.toml` in format 2: every retired key moved to the one that replaced
/// it, every comment and every other value kept, and each provider naming the
/// adapter format 1 inferred for it.
pub(super) fn config(text: &str) -> Result<String> {
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| Error::invalid(format!("{FILE}: {e}")))?;
    merge_table(&mut document, "inspect_files", "inspect_file");
    for (old, new) in ALIASES {
        let Some(value) = take(&mut document, old) else {
            continue;
        };
        let value = if old.ends_with("_ms") {
            match value.as_integer() {
                Some(ms) => toml_edit::value(format!("{ms}ms")),
                None => value,
            }
        } else {
            value
        };
        let unset = *old == "compact_at_tokens" && value.as_integer() == Some(0);
        if item(&document, new).is_none() && !unset {
            put(&mut document, new, value);
        }
    }
    for key in RETIRED {
        take(&mut document, key);
    }
    for key in OLD_TABLES {
        if item(&document, key)
            .and_then(toml_edit::Item::as_table_like)
            .is_some_and(|table| table.is_empty())
        {
            take(&mut document, key);
        }
    }
    if item(&document, "tools")
        .and_then(toml_edit::Item::as_table_like)
        .is_some_and(|table| table.is_empty())
    {
        take(&mut document, "tools");
    }
    rename_tools(&mut document);
    name_providers(&mut document);
    Ok(document.to_string())
}

fn configuration(settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    super::rewrite_configuration(settings, backup, plan, config)
}

fn item<'a>(document: &'a toml_edit::DocumentMut, path: &str) -> Option<&'a toml_edit::Item> {
    let mut current = document.as_item();
    for part in path.split('.') {
        current = current.as_table_like()?.get(part)?;
    }
    Some(current)
}

/// Remove the item at `path` and hand it back.
fn take(document: &mut toml_edit::DocumentMut, path: &str) -> Option<toml_edit::Item> {
    let (parent, last) = match path.rsplit_once('.') {
        Some((parent, last)) => (Some(parent), last),
        None => (None, path),
    };
    let mut current = document.as_item_mut();
    if let Some(parent) = parent {
        for part in parent.split('.') {
            current = current.as_table_like_mut()?.get_mut(part)?;
        }
    }
    current.as_table_like_mut()?.remove(last)
}

/// Set `path` to `value`, creating the tables on the way.
fn put(document: &mut toml_edit::DocumentMut, path: &str, value: toml_edit::Item) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = document.as_item_mut();
    for part in &parts[..parts.len() - 1] {
        let Some(table) = current.as_table_like_mut() else {
            return;
        };
        if table.get(part).is_none() {
            table.insert(part, toml_edit::table());
        }
        current = table.get_mut(part).expect("just inserted");
    }
    if let Some(table) = current.as_table_like_mut() {
        table.insert(parts[parts.len() - 1], value);
    }
}

/// Fold the table `old` into `new`: keys `new` already sets win. A new table
/// takes `old`'s place in the file.
fn merge_table(document: &mut toml_edit::DocumentMut, old: &str, new: &str) {
    let Some(toml_edit::Item::Table(mut moved)) = take(document, old) else {
        return;
    };
    match document
        .get_mut(new)
        .and_then(toml_edit::Item::as_table_like_mut)
    {
        Some(existing) => {
            let keys: Vec<String> = moved.iter().map(|(key, _)| key.to_string()).collect();
            for key in keys {
                if existing.get(&key).is_none() {
                    if let Some(value) = moved.remove(&key) {
                        existing.insert(&key, value);
                    }
                }
            }
        }
        None => {
            moved.set_implicit(false);
            document.insert(new, toml_edit::Item::Table(moved));
        }
    }
}

fn excluded_tools(document: &toml_edit::DocumentMut) -> Vec<String> {
    item(document, "classifier.excluded_tools")
        .and_then(toml_edit::Item::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(|name| name.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn rename_tools(document: &mut toml_edit::DocumentMut) {
    let Some(names) = document
        .get_mut("classifier")
        .and_then(toml_edit::Item::as_table_like_mut)
        .and_then(|table| table.get_mut("excluded_tools"))
        .and_then(toml_edit::Item::as_array_mut)
    else {
        return;
    };
    for name in names.iter_mut() {
        let renamed = name.as_str().and_then(|current| {
            TOOL_NAMES
                .iter()
                .find(|(old, _)| *old == current)
                .map(|(_, new)| *new)
        });
        if let Some(new) = renamed {
            let decor = name.decor().clone();
            *name = toml_edit::Value::from(new);
            *name.decor_mut() = decor;
        }
    }
}

/// Format 1 told a provider's adapter from its host or its name when the
/// configuration named none; format 2 never guesses. The guess format 1 made
/// is written down, so every provider keeps speaking the way it did.
fn name_providers(document: &mut toml_edit::DocumentMut) {
    let Some(list) = document
        .get_mut("providers")
        .and_then(toml_edit::Item::as_table_like_mut)
        .and_then(|table| table.get_mut("list"))
        .and_then(toml_edit::Item::as_array_of_tables_mut)
    else {
        return;
    };
    for provider in list.iter_mut() {
        if provider.contains_key("api") {
            continue;
        }
        let text = |key: &str| {
            provider
                .get(key)
                .and_then(toml_edit::Item::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let api = adapter_of(&text("base_url"), &text("dialect"), &text("name"));
        provider.insert("api", toml_edit::value(api));
    }
}

/// The adapter format 1 inferred for a provider that named none.
fn adapter_of(base_url: &str, dialect: &str, name: &str) -> &'static str {
    let host = base_url
        .parse::<ureq::http::Uri>()
        .ok()
        .and_then(|uri| uri.host().map(str::to_string))
        .unwrap_or_default();
    match host.as_str() {
        "api.openai.com" => "openai",
        "openrouter.ai" => "openrouter",
        "api.xiaomimimo.com" | "token-plan-cn.xiaomimimo.com" => "mimo",
        "api.anthropic.com" => "anthropic",
        "generativelanguage.googleapis.com" => "google",
        "api.x.ai" => "xai",
        "api.deepseek.com" => "deepseek",
        "api.groq.com" => "groq",
        "api.mistral.ai" => "mistral",
        _ if dialect == "anthropic" => "anthropic",
        _ if name == "9router" => "9router",
        _ if name == "ollama" => "ollama",
        _ => "compatible",
    }
}

// ── Layout ────────────────────────────────────────────────────────────────────

/// Every session row's project.
fn sessions_of(store: &Path) -> Result<BTreeMap<i64, PathBuf>> {
    let path = store.join("rune.db");
    let mut rows = BTreeMap::new();
    if !path.is_file() {
        return Ok(rows);
    }
    let db = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(std::time::Duration::from_secs(10))?;
    let mut statement = db.prepare("SELECT id, project FROM sessions")?;
    let found = statement.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in found {
        let (id, project) = row?;
        rows.insert(id, PathBuf::from(project));
    }
    Ok(rows)
}

/// Format 1's session folders, each with the project it belongs to — from
/// its database row, else from its own `meta.json` — or none.
fn old_sessions(store: &Path, rows: &BTreeMap<i64, PathBuf>) -> Vec<(PathBuf, Option<PathBuf>)> {
    entries(&store.join("sessions"))
        .into_iter()
        .filter(|dir| dir.is_dir())
        .map(|dir| {
            let project = name(&dir)
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .and_then(|id| {
                    rows.get(&id).cloned().or_else(|| {
                        std::fs::read(dir.join("meta.json"))
                            .ok()
                            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                            .and_then(|v| v["project"].as_str().map(PathBuf::from))
                    })
                });
            (dir, project)
        })
        .collect()
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|it| it.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    out.sort();
    out
}

fn name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn is_key(name: &str) -> bool {
    name.len() == 16
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn layout(store: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    let rows = sessions_of(store)?;
    // Format 1 named a project's memory, undo blobs and scratch by its key;
    // its project registry (`projects/<key>.json`) and the sessions' own
    // projects say which path each key was.
    let mut keys: BTreeMap<String, PathBuf> = BTreeMap::new();
    for file in entries(&store.join("projects")) {
        if file.extension().is_some_and(|e| e == "json") {
            if let Some((key, project)) = std::fs::read(&file)
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .and_then(|v| {
                    Some((
                        v["key"].as_str()?.to_string(),
                        PathBuf::from(v["project"].as_str()?),
                    ))
                })
            {
                keys.insert(key, project);
            }
        }
    }
    let sessions = old_sessions(store, &rows);
    for project in rows
        .values()
        .chain(sessions.iter().filter_map(|(_, p)| p.as_ref()))
    {
        keys.entry(crate::project::project_key(&crate::project::canonical(
            project,
        )))
        .or_insert_with(|| project.clone());
    }

    // Each project's folder, created now — the engine's own rule names it —
    // and remembered, so an undo removes it again.
    let mut folders: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    let mut folder_of = |project: &Path, plan: &mut Plan| -> Result<PathBuf> {
        let canonical = crate::project::canonical(project);
        if let Some(folder) = folders.get(&canonical) {
            return Ok(folder.clone());
        }
        let existed = crate::project::find(store, &canonical);
        let folder = crate::project::open(store, &canonical)?;
        if existed.is_none() {
            plan.created.push(folder.clone());
        }
        folders.insert(canonical, folder.clone());
        Ok(folder)
    };
    let obsolete = backup.join("obsolete");
    let set_aside = |path: &Path| obsolete.join(path.strip_prefix(store).unwrap_or(path));

    for (dir, project) in sessions {
        let to = match project {
            Some(project) => folder_of(&project, plan)?.join("sessions").join(name(&dir)),
            None => set_aside(&dir),
        };
        plan.moves.push((dir, to));
    }
    for file in entries(&store.join("sessions")) {
        if file.is_dir() {
            continue;
        }
        let to = if name(&file) == "Drafts.json" {
            store.join("drafts.json")
        } else {
            set_aside(&file)
        };
        plan.moves.push((file, to));
    }
    for kind in ["memory", "objects", "scratch"] {
        for dir in entries(&store.join(kind)) {
            let n = name(&dir);
            match keys.get(&n).filter(|_| is_key(&n)).cloned() {
                Some(project) => {
                    let folder = folder_of(&project, plan)?;
                    if kind == "scratch" {
                        for session in entries(&dir) {
                            let to = folder.join("scratch").join(name(&session));
                            plan.moves.push((session, to));
                        }
                        plan.emptied.push(dir);
                    } else {
                        plan.moves.push((dir, folder.join(kind)));
                    }
                }
                None => plan.moves.push((dir.clone(), set_aside(&dir))),
            }
        }
    }
    for path in [
        store.join("Capabilities"),
        store.join("sessions_index.json"),
        store.join("legacy"),
    ] {
        if path.exists() {
            plan.moves.push((path.clone(), set_aside(&path)));
        }
    }
    for file in entries(&store.join("projects")) {
        if file.is_file() && file.extension().is_some_and(|e| e == "json") {
            plan.moves.push((file.clone(), set_aside(&file)));
        }
    }
    plan.make_moves(backup)?;

    // What is left of format 1's folders is empty now.
    for dir in plan.emptied.clone() {
        std::fs::remove_dir(&dir)?;
    }
    for name in ["sessions", "memory", "objects", "scratch"] {
        let dir = store.join(name);
        if !dir.is_dir() {
            continue;
        }
        if std::fs::remove_dir(&dir).is_ok() {
            plan.emptied.push(dir);
            continue;
        }
        // What a person left there is kept, set aside like anything else
        // format 2 has no place for — never a reason to stop the upgrade.
        let aside = set_aside(&dir);
        plan.moves.push((dir.clone(), aside.clone()));
        plan.save(backup)?;
        if let Some(parent) = aside.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&dir, &aside)?;
    }
    plan.save(backup)
}

// ── Database ──────────────────────────────────────────────────────────────────

/// The schema migration, inside the upgrade: under its lock, with nobody else
/// in the database, and with its copy recorded so an undo can put it back.
pub(super) fn database(store: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    let path = store.join("rune.db");
    if !path.is_file() {
        return Ok(());
    }
    let schema: i64 = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?
    .query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if schema > 0 && schema < crate::store::SCHEMA_VERSION {
        // The store copies the database here before it migrates the schema.
        plan.database = Some(
            store
                .join("backups")
                .join(format!("BeforeSchema{}.db", crate::store::SCHEMA_VERSION)),
        );
        plan.save(backup)?;
    }
    let db = crate::store::Db::open(&path, std::time::Duration::from_secs(30))?;
    drop(db);
    Ok(())
}
