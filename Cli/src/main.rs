//! `rune` — a local coding agent.
//!
//! One binary, several faces: the interactive screen ([`ui`]), a one-shot prompt
//! for scripts, the line-delimited JSON host a GUI speaks to ([`host`]), and the
//! `-provider` / `-session` commands. All of them drive the engine
//! through the same protocol; see [`driver`].

mod args;
mod build;
mod driver;
mod handover;
mod host;
mod once;
mod report;
pub mod session_ipc;
mod ui;

use args::{Cmd, Model, Provider, SettingsCmd};
use runeengine::Command;

fn main() {
    // A command-line program is expected to die quietly when its reader goes
    // away: `rune -session | head` should print three lines and stop, not
    // panic about a broken pipe. And what the platform must note about this
    // process before anything changes, it notes now.
    runeengine::platform::at_start();

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match args::parse(&argv) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };
    // Before anything prints. The depth is resolved once and then fixed, so a
    // flag that arrived after the first byte would be a flag that did nothing.
    if parsed.no_color {
        ui::ansi::set_depth(ui::ansi::Depth::None);
    }
    if let Err(e) = run(parsed) {
        eprintln!("rune: {e}");
        std::process::exit(1);
    }
}

fn run(args: args::Args) -> Result<(), String> {
    match args.cmd {
        Cmd::Help => {
            print!("{}", args::USAGE);
            return Ok(());
        }
        // Asked of this build by a running one before it hands over: answered
        // without touching the home.
        Cmd::UpgradeCheck => return build::check(args.db.as_deref()),
        Cmd::Version => {
            println!("rune {}", runeengine::VERSION);
            return Ok(());
        }
        Cmd::SandboxRunner { ref pipe } => {
            return runeengine::platform::run_sandbox_runner(std::ffi::OsStr::new(pipe));
        }
        Cmd::Sandbox(args::SandboxCmd::AccountSetup { ref path }) => {
            return runeengine::platform::setup_sandbox_account(path);
        }
        _ => {}
    }

    // A window that `exec`d keeps the children the program before it started:
    // a session host that ended before the exec is a zombie only this process
    // can collect. Now, before this program starts any child of its own (see
    // `platform::reap_exited`) — and never in a host, whose ended background
    // tasks are its successor's to read.
    if matches!(args.cmd, Cmd::Ui | Cmd::Resume { .. }) {
        runeengine::platform::reap_exited();
        // Windows: Rune's own sandbox account is set up here, at a start a
        // person sees, once — a host or a one-shot run never asks for an
        // administrator.
        runeengine::platform::prepare_sandbox_account();
    }
    // Before anything reads the home: it is brought up to this build, or this
    // process goes back to the build that can read it.
    build::adopt(args.db.as_deref())?;
    ui::theme::init();
    // What a previous process of Rune handed this one, if it handed anything
    // over — taken now, after the home is this build's, and cleared from the
    // environment so nothing this process starts inherits it.
    let (mut handed, refused) = match handover::take() {
        Ok(handed) => (handed.unwrap_or_default(), None),
        Err(handover::Refused(why)) => (handover::Handover::default(), Some(why)),
    };

    if let Cmd::Config { ref words } = args.cmd {
        return report::config(words);
    }
    if let Cmd::Sandbox(what) = args.cmd {
        return report::sandbox(what);
    }

    if let Cmd::Capabilities {
        ref group,
        ref words,
    } = args.cmd
    {
        let home = runeengine::store::Db::home_dir().map_err(|e| e.to_string())?;
        let project = runeengine::platform::canonical(&std::path::PathBuf::from(
            driver::workspace(args.project.clone())?,
        ))
        .map_err(|e| e.to_string())?;
        let config = runeengine::settings::configuration::Configuration::load(&home)
            .map_err(|e| e.to_string())?;
        let limits = std::sync::Arc::new(
            runeengine::limits::Limits::load(&home).map_err(|e| e.to_string())?,
        );
        let registry = runeengine::tools::Registry::new(
            limits,
            std::sync::Arc::new(std::sync::Mutex::new(
                runeengine::settings::Settings::empty(),
            )),
        );
        let runtime = runeengine::capabilities::mcp::Runtime::new().map_err(|e| e.to_string())?;
        // Not a session: the project's own folder keeps what this command loads.
        let folder = runeengine::project::open(&home, &project).map_err(|e| e.to_string())?;
        let cap = runeengine::capabilities::Capabilities::open(
            &config,
            &project,
            0,
            folder.join("capabilities.json"),
            runtime,
            None,
        )
        .map_err(|e| e.to_string())?;
        let mut request = runeengine::capabilities::manage::Request {
            group: group.clone(),
            operation: words.first().cloned().unwrap_or("list".into()),
            arguments: words.iter().skip(1).cloned().collect(),
        };
        if (request.operation == "token" && request.arguments.len() == 1)
            || (request.operation == "secret" && request.arguments.len() == 2)
        {
            use std::io::Read;
            let mut token = String::new();
            std::io::stdin()
                .take(8193)
                .read_to_string(&mut token)
                .map_err(|e| e.to_string())?;
            request.arguments.push(token.trim_end().into());
        }
        let result = runeengine::capabilities::manage::execute(
            &cap,
            &registry,
            &request,
            &std::sync::atomic::AtomicBool::new(false),
            // The person's own command at a terminal, outside any session and
            // its mode: what it starts is what they asked to start.
            &|_, _| Ok(()),
        )
        .map_err(|e| e.to_string())?;
        println!(
            "{}",
            serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?
        );
        if group == "mcp" && request.operation == "login" {
            let server = request.arguments.first().ok_or("Missing MCP server")?;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let status = cap.login_status(server);
                if status["status"] != "auth-pending" {
                    println!("{}", status);
                    if status["status"] != "authenticated" {
                        return Err("MCP login did not complete".into());
                    }
                    break;
                }
            }
        }
        return Ok(());
    }
    let options = driver::Options {
        db: args.db.clone(),
        read_only: args.read_only,
        script: args.script.clone(),
        request_next: handed.window.as_ref().map(|w| w.request_next),
    };
    let mut d = driver::Driver::start(options)?;
    // The hosts the window before this one started and had not seen end.
    if let Some(window) = &handed.window {
        d.adopt_children(&window.children);
    }
    // Background tasks travel only between session hosts. Handed to anything
    // else, they would run on with nobody watching them: they are stopped.
    let tasks = std::mem::take(&mut handed.tasks);
    let tasks = if matches!(args.cmd, Cmd::Detached { .. } | Cmd::Host) {
        tasks
    } else {
        for error in d.engine.adopt_tasks(tasks) {
            eprintln!("rune: {error}");
        }
        Vec::new()
    };

    match args.cmd {
        Cmd::Help | Cmd::Version | Cmd::UpgradeCheck | Cmd::SandboxRunner { .. } => {
            unreachable!("answered above")
        }

        Cmd::Host => {
            let project = args.project.map(|p| p.to_string_lossy().into_owned());
            if let Some(why) = &refused {
                eprintln!("rune: the previous host's handover was not read: {why}");
            }
            host::run(d, project, handed.sessions, tasks, handed.input)
        }

        Cmd::Detached { session } => {
            let project = driver::workspace(args.project)?;
            if let Some(why) = &refused {
                eprintln!("rune: the previous host's handover was not read: {why}");
            }
            host::detached(d, project, session, handed.sessions, tasks, handed.owners)
        }

        Cmd::Ui => {
            let project = driver::workspace(args.project)?;
            ui::run(d, project, None, handed.window, refused)
        }

        Cmd::Resume { session } => {
            let project = driver::workspace(args.project)?;
            ui::run(d, project, Some(session), handed.window, refused)
        }

        Cmd::Once { text } => {
            let project = driver::workspace(args.project)?;
            once::run(d, project, text)
        }

        Cmd::Tools => {
            let out = d.call(|id| Command::ToolList { id })?;
            report::tools(&out);
            Ok(())
        }

        Cmd::Session(what) => match what {
            args::Session::List => {
                let project = driver::workspace(args.project.clone()).ok();
                if ui::term::is_tty() {
                    let picked = ui::picker::pick(&mut d, project.as_deref())?;
                    if let Some(session_id) = picked {
                        let project = driver::workspace(args.project)?;
                        return ui::run(d, project, Some(session_id), None, None);
                    }
                    Ok(())
                } else {
                    report::session(&mut d, what, args.project)
                }
            }
            _ => report::session(&mut d, what, args.project),
        },
        Cmd::Storage(what) => report::storage(&mut d, what),

        Cmd::Provider(what) => {
            match what {
                Provider::List => {
                    let out = d.call(|id| Command::ProviderList { id })?;
                    report::providers(&out);
                }
                Provider::Add {
                    name,
                    preset,
                    base_url,
                    key_name,
                    key,
                    model,
                } => {
                    eprintln!("Testing {name} before storing anything…");
                    let out = d.call(|id| Command::ProviderAdd {
                        id,
                        name,
                        preset,
                        base_url,
                        key_name,
                        key,
                        model,
                    })?;
                    report::provider_added(&out);
                }
                Provider::Remove { name } => {
                    d.call(|id| Command::ProviderRemove {
                        id,
                        name: name.clone(),
                    })?;
                    println!("Removed {name}. Its key is still stored — `rune -provider key` replaces it.");
                }
                Provider::Key { name, key } => {
                    let out = d.call(|id| Command::ProviderKey {
                        id,
                        name: name.clone(),
                        key_name: None,
                        key,
                    })?;
                    let held = out["keys"].as_u64().unwrap_or(1);
                    println!("Stored the key for {name} encrypted in ~/.rune/secrets.vault (0600, key in ~/.rune/master.key).");
                    if held > 1 {
                        println!(
                            "{name} now holds {held} keys. A rate-limited one moves to the back \
                             and the next is used automatically."
                        );
                    }
                }
                Provider::Timeout { name, seconds } => {
                    d.call(|id| Command::ProviderTimeout {
                        id,
                        name: name.clone(),
                        seconds,
                    })?;
                    match seconds {
                        Some(s) => println!("{name} calls may now take up to {s}s."),
                        None => println!("{name} is back to the default timeout."),
                    }
                }
                Provider::DryRun { name, model } => {
                    let out = d.call(|id| Command::ProviderDryRun { id, name, model })?;
                    println!("POST {}", out["url"].as_str().unwrap_or("?"));
                    for h in out["headers"].as_array().into_iter().flatten() {
                        println!(
                            "  {}: {}",
                            h["header"].as_str().unwrap_or("?"),
                            h["value"].as_str().unwrap_or("?")
                        );
                    }
                    println!(
                        "\n{}",
                        serde_json::to_string_pretty(&out["body"]).unwrap_or_default()
                    );
                    match out["timeout"].as_u64() {
                        Some(s) => println!("\nTime allowed: {s}s. Nothing was sent."),
                        None => println!("\nTime allowed: the default. Nothing was sent."),
                    }
                }
                Provider::Keys { name } => {
                    let out = d.call(|id| Command::ProviderKeys {
                        id,
                        name: name.clone(),
                    })?;
                    println!(
                        "{name} — {} key(s), {} usable now",
                        out["total"].as_u64().unwrap_or(0),
                        out["ready"].as_u64().unwrap_or(0)
                    );
                    for k in out["keys"].as_array().into_iter().flatten() {
                        println!(
                            "  {}. {}",
                            k["position"].as_u64().unwrap_or(0),
                            k["masked"].as_str().unwrap_or("?")
                        );
                    }
                }
                Provider::NewStore { confirmed: false } => {
                    return Err(
                        "this sets Rune's credential store and its master.key aside into the \
                         backups folder and starts with no keys — every key must be added again. \
                         Restoring the matching master.key or secrets.vault from a backup keeps \
                         them instead. To go ahead: rune -provider new-store --yes"
                            .into(),
                    );
                }
                Provider::NewStore { confirmed: true } => {
                    let out = d.call(|id| Command::CredentialsNew { id })?;
                    println!(
                        "The old credential store was set aside in {}. Add keys again in Settings › Connections or with rune -provider key <name> <key>.",
                        out["set_aside"].as_str().unwrap_or("the backups folder")
                    );
                }
                Provider::DropKey { name, position } => {
                    let out = d.call(|id| Command::ProviderDropKey {
                        id,
                        name: name.clone(),
                        position,
                    })?;
                    println!(
                        "Removed key {position}. {name} now holds {} key(s).",
                        out["keys"].as_u64().unwrap_or(0)
                    );
                }
                Provider::Default { name } => {
                    d.call(|id| Command::ProviderDefault {
                        id,
                        name: name.clone(),
                    })?;
                    println!("New sessions will use {name}.");
                }
                Provider::Test { name, model } => {
                    let out = d.call(|id| Command::ProviderTest { id, name, model })?;
                    if let Some(m) = out["model"].as_str() {
                        println!("{} · {m} — works.", out["provider"].as_str().unwrap_or("?"),);
                    } else {
                        let count = out["models_count"].as_u64().unwrap_or(0);
                        println!(
                            "{} — connection verified ({count} models available).",
                            out["provider"].as_str().unwrap_or("?"),
                        );
                    }
                }
                Provider::Models { name } => {
                    let out = d.call(|id| Command::ProviderModels { id, name })?;
                    report::models(&out);
                }
            }
            Ok(())
        }

        Cmd::Model(_) | Cmd::Settings(_) => {
            let project = driver::workspace(args.project)?;
            let session = session_here(&mut d, &project)?;
            let (list_models, update) = match args.cmd {
                Cmd::Model(Model::List) => (true, None),
                Cmd::Model(Model::Set { id }) => (false, Some(("model".into(), id))),
                Cmd::Settings(SettingsCmd::Show) => (false, None),
                Cmd::Settings(SettingsCmd::Set { key, value }) => (false, Some((key, value))),
                _ => unreachable!("settings/model handler"),
            };
            let out = if let Some(session) = session {
                match update {
                    Some((key, value)) => d.call(|id| Command::SettingsSet {
                        id,
                        session,
                        key,
                        value,
                    })?,
                    None => d.call(|id| Command::SettingsGet { id, session })?,
                }
            } else {
                let opened = d.call(|id| Command::DraftOpen {
                    id,
                    project: project.clone(),
                    token: None,
                })?;
                let mut draft: runeengine::store::drafts::Draft =
                    serde_json::from_value(opened["draft"].clone())
                        .map_err(|error| error.to_string())?;
                let changed = update.is_some();
                let (key, value) =
                    update.map_or((None, String::new()), |(key, value)| (Some(key), value));
                let out = d.call(|id| Command::DraftSettings {
                    id,
                    draft: Box::new(draft.clone()),
                    key,
                    value,
                    provider: None,
                    custom: false,
                })?;
                if changed {
                    draft.composer.settings = serde_json::from_value(out["settings"].clone())
                        .map_err(|error| error.to_string())?;
                    draft.composer.preference_changed = true;
                    d.call(|id| Command::DraftSave {
                        id,
                        base: draft.composer.revision,
                        composer: Box::new(draft.composer.clone()),
                        draft: Box::new(draft),
                    })?;
                }
                out
            };
            if list_models {
                let provider = out["provider"].as_str().unwrap_or_default().to_string();
                let out = d.call(|id| Command::ProviderModels { id, name: provider })?;
                report::models(&out);
            } else {
                report::settings(&out);
            }
            Ok(())
        }

        Cmd::Capabilities { .. } | Cmd::Config { .. } | Cmd::Sandbox(_) => {
            unreachable!("handled before engine startup")
        }

        Cmd::Doctor => {
            let project = driver::workspace(args.project)?;
            report::doctor(&mut d, &project)
        }
    }
}

/// The latest committed settings scope, if this workspace has a conversation.
/// An unstarted workspace uses draft/application settings without a session ID.
fn session_here(d: &mut driver::Driver, project: &str) -> Result<Option<i64>, String> {
    match d.newest(project)? {
        Some(id) => d
            .resume(id, Some(project.to_string()))
            .map(|(id, _)| Some(id)),
        None => Ok(None),
    }
}
