//! What the non-interactive commands print.
//!
//! Plain text, aligned by columns, and coloured only when a terminal is on the
//! other end — `rune -session | grep` must produce something greppable, not
//! escape codes. Every function here takes the protocol's own JSON, so the
//! terminal and a GUI are formatting the same answer rather than two.

use runeengine::Command;
use serde_json::Value;

use crate::args::{SandboxCmd, Session, Storage};
use crate::driver::Driver;
use crate::ui::ansi;

/// Colour, unless this is a pipe.
fn tint(colour: ansi::Ink, text: &str) -> String {
    if crate::ui::term::is_tty() {
        format!("{colour}{text}{}", ansi::RESET)
    } else {
        text.to_string()
    }
}

fn dim(text: &str) -> String {
    tint(ansi::MUTED, text)
}

pub fn tools(out: &Value) {
    for tool in out["tools"].as_array().map(|v| v.as_slice()).unwrap_or(&[]) {
        let name = tool["name"].as_str().unwrap_or("?");
        let writes = tool["mutates"].as_bool().unwrap_or(false);
        println!(
            "{:<14} {}  {}",
            name,
            if writes {
                tint(ansi::WARN, "changes")
            } else {
                dim("reads  ")
            },
            dim(tool["description"]
                .as_str()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or(""))
        );
    }
}

pub fn session(
    d: &mut Driver,
    what: Session,
    project: Option<std::path::PathBuf>,
) -> Result<(), String> {
    match what {
        Session::List => {
            let here = crate::driver::workspace(project).ok();
            let out = d.call(|id| Command::SessionList {
                id,
                project: here.clone(),
            })?;
            let list = out["sessions"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!("No sessions yet.");
                return Ok(());
            }
            for s in list {
                let size_bytes = s["size_bytes"].as_u64().unwrap_or(0);
                let size_str = crate::ui::picker::format_session_size(size_bytes);
                println!(
                    "{:>5}  {:>8}  {:<35} {}",
                    s["id"].as_i64().unwrap_or(0),
                    size_str,
                    s["title"].as_str().unwrap_or("(untitled)"),
                    dim(s["project"].as_str().unwrap_or(""))
                );
            }
        }
        Session::Rename { id: session, title } => {
            d.call(|id| Command::SessionRename {
                id,
                session,
                title: title.clone(),
            })?;
            println!("Renamed session {session} to {title:?}.");
        }
        Session::Delete { id: session } => {
            d.call(|id| Command::SessionDelete { id, session })?;
            println!("Deleted session {session}, its undo history and its scratch.");
        }
    }
    Ok(())
}

/// What Rune keeps for each project, and — after the person confirms the
/// exact list — removing what belonged to projects whose path is gone.
/// What the sandbox left on disk, and taking it back (`rune -sandbox`).
/// Windows lets a confined command in by access entries on the folders it
/// may reach; each was recorded before it was written, and every one is
/// taken back here. Landlock and Seatbelt leave nothing behind.
pub fn sandbox(what: SandboxCmd) -> Result<(), String> {
    use runeengine::platform::{self, Os};
    if let SandboxCmd::AccountSetup { ref path } = what {
        return platform::setup_sandbox_account(path);
    }
    let kernel = match platform::OS {
        Os::Linux => Some("Landlock"),
        Os::MacOs => Some("Seatbelt"),
        Os::Windows => None,
    };
    if let Some(kernel) = kernel {
        println!(
            "Nothing to {}: {kernel} confines a command in the kernel and leaves nothing on disk.",
            match what {
                SandboxCmd::List => "show",
                SandboxCmd::Reset | SandboxCmd::AccountSetup { .. } => "take back",
                SandboxCmd::Setup => "set up",
            }
        );
        return Ok(());
    }
    let home = runeengine::store::Db::home_dir().map_err(|e| e.to_string())?;
    let ledger = runeengine::sandbox::grants_ledger(&home);
    let entries = |n: usize| if n == 1 { "entry" } else { "entries" };
    match what {
        SandboxCmd::AccountSetup { ref path } => {
            return platform::setup_sandbox_account(path);
        }
        SandboxCmd::Setup => {
            platform::set_up_sandbox_account()?;
            println!(
                "Rune's sandbox account is set up: confined commands run under it from their next session."
            );
        }
        SandboxCmd::List => {
            let recorded = platform::recorded_grants(&ledger)
                .map_err(|e| format!("{}: {e}", ledger.display()))?;
            if recorded.is_empty() {
                println!("No access entry of Rune's sandbox is on disk.");
                return Ok(());
            }
            for entry in &recorded {
                let access = if entry.write { "write" } else { "read" };
                println!("{access:<5}  {}", entry.path.display());
            }
            println!(
                "\n{} access {} let Rune's confined commands in. Take them back: rune -sandbox reset",
                recorded.len(),
                entries(recorded.len())
            );
        }
        SandboxCmd::Reset => {
            let forgotten = platform::forget_grants(&ledger)
                .map_err(|e| format!("{}: {e}", ledger.display()))?;
            println!(
                "Took back {} access {}.",
                forgotten.removed,
                entries(forgotten.removed)
            );
            if forgotten.absent > 0 {
                println!(
                    "{} more {} gone already.",
                    forgotten.absent,
                    if forgotten.absent == 1 { "was" } else { "were" }
                );
            }
            for (entry, why) in &forgotten.kept {
                println!(
                    "{} {}: {why}",
                    tint(ansi::WARN, "kept"),
                    entry.path.display()
                );
            }
            if !forgotten.kept.is_empty() {
                println!("What was kept stays recorded; run the reset again to retry.");
            }
            if let Err(err) = platform::remove_sandbox_account() {
                eprintln!("warning: sandbox account cleanup: {err}");
            }
            println!("A Rune still running lets its commands in again at their next start.");
        }
    }
    Ok(())
}

pub fn storage(d: &mut Driver, what: Storage) -> Result<(), String> {
    use runeengine::proto::{StorageFolder, StoragePruned};
    let out = d.call(|id| Command::StorageList { id })?;
    let folders: Vec<StorageFolder> =
        serde_json::from_value(out["folders"].clone()).map_err(|e| e.to_string())?;
    let size = runeengine::media::human;
    let show = |folder: &StorageFolder| {
        let state = if folder.present {
            dim("present")
        } else {
            tint(ansi::WARN, "missing")
        };
        println!(
            "{:>9}  {:>3} {}  {}  {}",
            size(folder.bytes),
            folder.sessions,
            if folder.sessions == 1 {
                "session "
            } else {
                "sessions"
            },
            state,
            folder.project
        );
        println!("{:>34}{}", "", dim(&format!("projects/{}", folder.folder)));
    };
    match what {
        Storage::List => {
            if folders.is_empty() {
                println!("No project folders yet.");
                return Ok(());
            }
            for folder in &folders {
                show(folder);
            }
            let total: u64 = folders.iter().map(|f| f.bytes).sum();
            let missing = folders.iter().filter(|f| !f.present).count();
            println!(
                "\n{} in {} project {}.",
                size(total),
                folders.len(),
                if folders.len() == 1 {
                    "folder"
                } else {
                    "folders"
                }
            );
            match missing {
                0 => {}
                1 => println!("1 belongs to a path that no longer exists: rune -storage prune"),
                n => println!("{n} belong to paths that no longer exist: rune -storage prune"),
            }
        }
        Storage::Prune => {
            let gone: Vec<&StorageFolder> = folders.iter().filter(|f| !f.present).collect();
            if gone.is_empty() {
                println!("Nothing to prune: every project folder's path still exists.");
                return Ok(());
            }
            for folder in &gone {
                show(folder);
            }
            let sessions: usize = gone.iter().map(|f| f.sessions).sum();
            let bytes: u64 = gone.iter().map(|f| f.bytes).sum();
            print!(
                "\nDelete {} ({}): {} {}, undo history, memory and grants? \
                 This cannot be undone. [y/N] ",
                if gone.len() == 1 {
                    "this folder".to_string()
                } else {
                    format!("these {} folders", gone.len())
                },
                size(bytes),
                sessions,
                if sessions == 1 { "session" } else { "sessions" }
            );
            use std::io::Write;
            let _ = std::io::stdout().flush();
            let mut answer = String::new();
            let _ = std::io::stdin().read_line(&mut answer);
            if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                println!("Nothing was deleted.");
                return Ok(());
            }
            let names: Vec<String> = gone.iter().map(|f| f.folder.clone()).collect();
            let out = d.call(|id| Command::StoragePrune {
                id,
                folders: names.clone(),
            })?;
            let pruned: StoragePruned = serde_json::from_value(out).map_err(|e| e.to_string())?;
            println!(
                "Deleted {} {}, {} freed.",
                pruned.removed.len(),
                if pruned.removed.len() == 1 {
                    "folder"
                } else {
                    "folders"
                },
                size(pruned.bytes_freed)
            );
            for kept in &pruned.kept {
                println!("Kept projects/{}: {}", kept.folder, kept.reason);
            }
        }
    }
    Ok(())
}

/// The credential store, when it cannot be opened: said first, with what to
/// do, on every screen that lists keys.
fn credentials(out: &Value) {
    if out["credentials"]["state"] == "unavailable" {
        println!(
            "{} {}\n",
            tint(ansi::REMOVED, "CREDENTIALS UNAVAILABLE"),
            out["credentials"]["note"].as_str().unwrap_or("")
        );
    }
}

/// What a connection's key is, as the engine said it: one word for the
/// column, and the sentence when it is not simply there.
fn key_word(entry: &Value) -> (&'static str, ansi::Ink, Option<String>) {
    let note = entry["key_note"].as_str().map(String::from);
    match entry["key_state"].as_str() {
        Some("present") => ("key set", ansi::OK, None),
        Some("missing") => ("key missing", ansi::REMOVED, note),
        Some("unavailable") => ("key unavailable", ansi::REMOVED, note),
        _ => ("no key needed", ansi::MUTED, None),
    }
}

pub fn providers(out: &Value) {
    credentials(out);
    let configured = out["providers"].as_array().cloned().unwrap_or_default();
    if configured.is_empty() {
        println!("Nothing configured yet.\n");
    } else {
        println!("{}", tint(ansi::ACCENT, "CONFIGURED"));
        for p in &configured {
            let name = p["name"].as_str().unwrap_or("?");
            let is_default = p["default"].as_bool().unwrap_or(false);
            let (key_str, key_tint, key_note) = key_word(p);
            let plain_status_len = key_str.len() + if is_default { 10 } else { 0 };
            let pad = 22usize.saturating_sub(plain_status_len);
            let padding = " ".repeat(pad);
            let default_tag = if is_default {
                format!(" {}", tint(ansi::ACCENT, "(default)"))
            } else {
                String::new()
            };
            let status_disp = format!("{}{}{}", tint(key_tint, key_str), default_tag, padding);
            let base_url = dim(p["base_url"].as_str().unwrap_or(""));
            println!("  {name:<12} {status_disp} {base_url}");
            if let Some(note) = key_note {
                println!("  {:<12} {}", "", dim(&note));
            }

            let models: Vec<&str> = p["models"]
                .as_array()
                .map(|m| m.iter().filter_map(|m| m["id"].as_str()).collect())
                .unwrap_or_default();
            if !models.is_empty() {
                if models.len() <= 4 {
                    println!("               models: {}", dim(&models.join(", ")));
                } else {
                    let count = models.len();
                    let sample = models
                        .iter()
                        .take(3)
                        .copied()
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!(
                        "               {} models ({sample}, …) · {}",
                        count,
                        dim(&format!("rune -provider models {name}"))
                    );
                }
            }
        }
        println!();
    }
    println!("{}", tint(ansi::ACCENT, "AVAILABLE"));
    for p in out["presets"]
        .as_array()
        .map(|v| v.as_slice())
        .unwrap_or(&[])
    {
        println!(
            "  {:<12} {}",
            p["name"].as_str().unwrap_or("?"),
            dim(p["key_hint"].as_str().unwrap_or(""))
        );
    }
    println!("\n  {}", dim("rune -provider add <name> -key <key>"));
    services(out);
}

/// What one connection serves: each service on with its route, and a
/// gateway's other kept routes — kept whether the service is on or off.
fn serves(s: &Value) -> String {
    let mut serves = Vec::new();
    for service in ["search", "fetch"] {
        let kept: Vec<&str> = s["routes"][service]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let route = s[service]["route"].as_str();
        let others: Vec<&str> = kept.iter().copied().filter(|r| Some(*r) != route).collect();
        let also = if others.is_empty() {
            String::new()
        } else {
            format!(", also kept: {}", others.join(", "))
        };
        match (s[service].is_null(), route) {
            (false, Some(route)) => serves.push(format!("{service} ({route}{also})")),
            (false, None) => serves.push(service.to_string()),
            (true, _) if !kept.is_empty() => {
                serves.push(format!("{service} off (kept: {})", kept.join(", ")))
            }
            (true, _) => {}
        }
    }
    serves.join(", ")
}

/// The connections that serve the web tools, and who serves each now.
fn services(out: &Value) {
    println!("\n{}", tint(ansi::ACCENT, "SEARCH & FETCH"));
    let list = out["services"].as_array().cloned().unwrap_or_default();
    for s in &list {
        let keys = match key_word(s) {
            (_, _, Some(note)) => note,
            ("no key needed", ..) => "no key needed".into(),
            _ => format!("{} key(s)", s["keys"].as_u64().unwrap_or(0)),
        };
        println!(
            "  {:<12} {}  {}",
            s["name"].as_str().unwrap_or("?"),
            serves(s),
            dim(&keys)
        );
    }
    for (label, service) in [("web search", "search"), ("web fetch", "fetch")] {
        let now = out["web"][service].as_str().unwrap_or("");
        println!(
            "  {label}: {}",
            if now.starts_with("unavailable") {
                dim(now)
            } else {
                tint(ansi::OK, now)
            }
        );
    }
    if list.is_empty() {
        println!(
            "  {}",
            dim("add one in Settings › Connections (Add connection → Search & Fetch)")
        );
    }
}

pub fn provider_added(out: &Value) {
    println!(
        "Added {} at {}{}.",
        out["added"].as_str().unwrap_or("?"),
        out["base_url"].as_str().unwrap_or("?"),
        out["model"]
            .as_str()
            .map(|m| format!(" · tested with {m}"))
            .unwrap_or_default()
    );
    if out["default"].as_bool().unwrap_or(false) {
        println!("New sessions will use it.");
    }
}

pub fn models(out: &Value) {
    let available = out["available"].as_array().cloned().unwrap_or_default();
    if available.is_empty() {
        println!("That provider lists no models.");
        return;
    }
    for m in available {
        println!("  {}", m.as_str().unwrap_or_default());
    }
}

pub fn settings(out: &Value) {
    let s = &out["settings"];
    println!(
        "{} {} · {}",
        tint(ansi::ACCENT, "MODEL"),
        s["model"].as_str().unwrap_or("(none chosen)"),
        s["provider"].as_str().unwrap_or("(no provider)")
    );
    // Three kinds of row, and the panel draws the same three. Two views of one
    // setting must not disagree about what it is.
    let mut missing: Vec<(&str, &str)> = Vec::new();
    for knob in out["knobs"].as_array().map(|v| v.as_slice()).unwrap_or(&[]) {
        let key = knob["key"].as_str().unwrap_or("");
        let label = knob["label"].as_str().unwrap_or(key);
        let entry = out["suppressed"]
            .as_array()
            .and_then(|l| l.iter().find(|x| x["key"].as_str() == Some(key)));
        let reason = entry.and_then(|x| x["reason"].as_str()).unwrap_or("");
        match entry.map(|x| x["permanent"].as_bool().unwrap_or(false)) {
            // The endpoint has no such control: not a setting with a dash, and
            // not something to print among the ones that work.
            Some(true) => missing.push((label, reason)),
            // A real setting, closed by another one. The reason says which.
            Some(false) => {
                println!("  {label:<20} {:<10}{}", "—", dim(&format!("  ({reason})")))
            }
            None => println!("  {label:<20} {:<10}", crate::ui::panels::value_of(s, key)),
        }
    }
    if !missing.is_empty() {
        println!("\n{}", dim("NOT ON THIS PROVIDER"));
        for (label, reason) in missing {
            println!("  {}", dim(&format!("{label:<20} {reason}")));
        }
    }
    if let Some(ceiling) = out["max_output_ceiling"].as_u64() {
        println!(
            "\n{}",
            dim(&format!("max_output ceiling for this model: {ceiling}"))
        );
    }
    println!("{}", dim("change one with: rune -set <key>=<value>"));
}

pub fn config(words: &[String]) -> Result<(), String> {
    use runeengine::settings::configuration::{Configuration, FILE};
    let home = runeengine::store::Db::home_dir().map_err(|e| e.to_string())?;
    let args: Vec<_> = words.iter().map(String::as_str).collect();
    let outcome = (|| -> runeengine::Result<String> {
        match args.as_slice(){
            []=>Ok(format!("{}\nGlobal settings and providers: {FILE}\nCredentials: secrets.vault (encrypted, key in ~/.rune/master.key)",home.display())),
            ["check"]=>{
                Configuration::load(&home)?;
                Ok("Configuration is valid.".into())
            },
            ["show"]=>Configuration::load(&home)?.display_toml(),
            ["defaults"]=>Ok(Configuration::reference()),
            ["explain",key]=>Configuration::load(&home)?.explain(key),
            ["set",key,value]=>{Configuration::edit(&home,Some(key),Some(Configuration::parse_value(key,value)?),false, None)?;Ok(format!("Saved {key} to {}.",home.join(FILE).display()))},
            ["reset",key]=>{Configuration::edit(&home,Some(key),None,false, None)?;Ok(format!("Reset {key} to its default or inheritance."))},
            ["format"]=>{Configuration::edit(&home,None,None,false,None)?;Ok(format!("Formatted {} with grouped settings. Existing values were preserved.",home.join(FILE).display()))},
            _=>Err(runeengine::Error::invalid("use -config show|check|defaults|format|explain <key>|set <key> <value>|reset <key>"))
        }
    })();
    println!("{}", outcome.map_err(|e| e.to_string())?);
    Ok(())
}

/// Everything that could be wrong with an install, checked in order.
pub fn doctor(d: &mut Driver, project: &str) -> Result<(), String> {
    let mut problems = 0;

    println!("{}", tint(ansi::ACCENT, "WORKSPACE"));
    println!("  {project}");

    println!("\n{}", tint(ansi::ACCENT, "SANDBOX"));
    let session = d.open(project)?;
    let sandbox = d.engine.sandbox_of(session).map_err(|e| e.to_string())?;
    let report = sandbox.report();
    if report.enforced {
        println!(
            "  {} landlock abi {}",
            tint(ansi::OK, "enforced"),
            report.abi
        );
    } else {
        problems += 1;
        println!(
            "  {} {}",
            tint(ansi::WARN, "not enforced"),
            report.reason.clone().unwrap_or_default()
        );
    }
    for escape in &report.escapes {
        problems += 1;
        println!("  {} {}", tint(ansi::REMOVED, "way out:"), escape.detail);
        println!("            {}", dim(&escape.fix));
    }

    println!("\n{}", tint(ansi::ACCENT, "PROVIDERS"));
    let list = d.call(|id| Command::ProviderList { id })?;
    let configured = list["providers"].as_array().cloned().unwrap_or_default();
    if configured.is_empty() {
        problems += 1;
        println!(
            "  {} — rune -provider add mimo -key <key>",
            tint(ansi::REMOVED, "none configured")
        );
    }
    for p in configured {
        let name = p["name"].as_str().unwrap_or("?").to_string();
        if let (_, _, Some(note)) = key_word(&p) {
            problems += 1;
            println!("  {name:<12} {}", tint(ansi::REMOVED, &note));
            continue;
        }
        match d.call(|id| Command::ProviderTest {
            id,
            name: name.clone(),
            model: None,
        }) {
            Ok(out) => {
                let info = if let Some(m) = out["model"].as_str().filter(|s| !s.is_empty()) {
                    dim(m)
                } else if let Some(c) = out["models_count"].as_u64() {
                    dim(&format!("{c} models"))
                } else {
                    String::new()
                };
                println!("  {name:<12} {} {info}", tint(ansi::OK, "reachable"));
            }
            Err(e) => {
                problems += 1;
                println!("  {name:<12} {} {}", tint(ansi::REMOVED, "failed"), dim(&e));
            }
        }
    }

    println!("\n{}", tint(ansi::ACCENT, "SEARCH & FETCH"));
    for s in list["services"].as_array().cloned().unwrap_or_default() {
        let name = s["name"].as_str().unwrap_or("?").to_string();
        for service in ["search", "fetch"] {
            if s[service].is_null() {
                continue;
            }
            match d.call(|id| Command::ServiceTest {
                id,
                name: name.clone(),
                service: service.into(),
            }) {
                Ok(_) => println!("  {name:<12} {} {}", tint(ansi::OK, "works"), dim(service)),
                Err(e) => {
                    problems += 1;
                    println!("  {name:<12} {} {}", tint(ansi::REMOVED, "failed"), dim(&e));
                }
            }
        }
    }
    for (label, service) in [("web search", "search"), ("web fetch", "fetch")] {
        println!(
            "  {label}: {}",
            dim(list["web"][service].as_str().unwrap_or(""))
        );
    }

    println!();
    if problems == 0 {
        println!("{}", tint(ansi::OK, "Everything checks out."));
    } else {
        println!(
            "{}",
            tint(ansi::WARN, &format!("{problems} thing(s) to look at."))
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_connection_says_what_it_serves_and_the_routes_it_keeps() {
        let router = json!({"search":{"added":1,"route":"ag"},"fetch":null,
            "routes":{"search":["ag","glm"],"fetch":["exa"]}});
        assert_eq!(
            serves(&router),
            "search (ag, also kept: glm), fetch off (kept: exa)"
        );
        let tavily = json!({"search":{"added":1,"route":null},"fetch":{"added":2,"route":null},
            "routes":{"search":[],"fetch":[]}});
        assert_eq!(serves(&tavily), "search, fetch");
    }
}
