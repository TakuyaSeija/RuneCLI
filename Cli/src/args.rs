//! The command surface: `rune`, and `rune -<command> …`.
//!
//! Hand-rolled, as before, and now for a better reason than saving a
//! dependency: the shape is unusual on purpose. The owner wanted single-dash
//! commands (`rune -provider add`) rather than the git-style bare subcommands an
//! argument crate assumes, and bending a crate into a shape it dislikes costs
//! more than eighty lines of matching.
//!
//! Everything is a **command word after a flag**, so `-provider add mimo` reads
//! as one thought. Modifiers (`-project`, `-db`, `-read-only`) may appear with
//! any command, because "which workspace" is a different question from "what to
//! do".

use std::path::PathBuf;

#[derive(Debug, PartialEq)]
pub struct Args {
    pub cmd: Cmd,
    /// Workspace to act in. Defaults to the current directory.
    pub project: Option<PathBuf>,
    pub db: Option<PathBuf>,
    pub read_only: bool,
    /// Print without colour, whatever the terminal claims. `NO_COLOR` in the
    /// environment does the same thing; this is the version you can put in an
    /// alias.
    pub no_color: bool,
    /// Replay a recorded script instead of calling a model. What the protocol
    /// tests drive the host with, and the only way to reproduce a session
    /// exactly.
    pub script: Option<PathBuf>,
}

#[derive(Debug, PartialEq)]
pub enum Cmd {
    /// The interactive terminal app. The default.
    Ui,
    /// One prompt, printed as it happens, then exit. For scripts and pipes.
    Once {
        text: String,
    },
    /// The line-delimited JSON host a GUI speaks to.
    Host,
    /// Private IPC host: its owning UI may close without cancelling its turns.
    Detached {
        session: i64,
    },
    Resume {
        session: i64,
    },
    Session(Session),
    /// What Rune keeps for each project, and pruning what belonged to a
    /// project that no longer exists.
    Storage(Storage),
    /// What the sandbox left on disk, and taking it back.
    Sandbox(SandboxCmd),
    Provider(Provider),
    Model(Model),
    Settings(SettingsCmd),
    Tools,
    /// Where configuration lives, and what is in it.
    Config {
        words: Vec<String>,
    },
    Capabilities {
        group: String,
        words: Vec<String>,
    },
    /// Everything that could be wrong, checked and reported.
    Doctor,
    /// Whether this build could take the home over, without changing it —
    /// what a running build asks of a newly installed one.
    UpgradeCheck,
    Version,
    Help,
}

#[derive(Debug, PartialEq)]
pub enum Session {
    List,
    Rename { id: i64, title: String },
    Delete { id: i64 },
}

#[derive(Debug, PartialEq)]
pub enum Storage {
    List,
    Prune,
}

#[derive(Debug, PartialEq)]
pub enum SandboxCmd {
    /// Every access entry it recorded (Windows; nothing elsewhere).
    List,
    /// Take every one back.
    Reset,
}

#[derive(Debug, PartialEq)]
pub enum Provider {
    List,
    Add {
        name: String,
        preset: Option<String>,
        base_url: Option<String>,
        key_name: Option<String>,
        key: Option<String>,
        model: Option<String>,
    },
    Remove {
        name: String,
    },
    Key {
        name: String,
        key: String,
    },
    Keys {
        name: String,
    },
    DryRun {
        name: String,
        model: Option<String>,
    },
    Timeout {
        name: String,
        seconds: Option<u64>,
    },
    DropKey {
        name: String,
        position: usize,
    },
    /// Set an unreadable credential store aside and start a new one; only
    /// with `--yes`, the person's explicit choice.
    NewStore {
        confirmed: bool,
    },
    Default {
        name: String,
    },
    Test {
        name: String,
        model: Option<String>,
    },
    Models {
        name: String,
    },
}

#[derive(Debug, PartialEq)]
pub enum Model {
    List,
    Set { id: String },
}

#[derive(Debug, PartialEq)]
pub enum SettingsCmd {
    Show,
    Set { key: String, value: String },
}

pub const USAGE: &str = "\
rune — a local coding agent that runs on this machine

CAPABILITIES
    rune -skills [list|show|enable|disable|install|reload] [arguments]
    rune -mcp [list|add|approve|validate|test|token] [arguments]
    Add --scope user|project|private-project to configuration changes

USAGE
    rune [dir]                         open the workspace here (or in dir)
    rune -p \"fix the login bug\"        run one prompt and exit

SESSIONS
    rune -session                      interactive session picker (or list in pipe)
    rune -resume <id>                  resume a session by id
    rune -session rename <id> <title>  rename a session
    rune -session delete <id>          permanently delete a session

STORAGE
    rune -storage                      every project folder: path, size, sessions
    rune -storage prune                delete what belonged to projects that are gone
    rune -sandbox                      access entries the sandbox wrote (Windows only)
    rune -sandbox reset                take every one of them back

PROVIDER & MODEL
    rune -provider list                list connections (models, search, fetch) and presets
    rune -provider add <name> -key <key> [-preset <p>] [-base <url>] [-model <m>]
    rune -provider key <name> <key>    add an API key to a connection's ring
    rune -provider keys <name>         list that provider's keys, masked
    rune -provider dropkey <name> <n>  remove key number <n>
    rune -provider new-store --yes     set an unreadable key store aside, start anew
    rune -provider default <name>      set active default provider
    rune -provider remove <name>       remove provider configuration
    rune -provider test <name>         send a test request to verify it works
    rune -provider models <name>       query provider for available models
    rune -provider dryrun <name> [m]   show request payload without sending
    rune -provider timeout <n> <secs>  call timeout in seconds (`default` to clear)
    rune -model list                   list available models for current provider
    rune -model set <id>               set active model for workspace
    rune -settings                     what is in force for this workspace
    rune -set <key>=<value>            temperature, top_p, max_output, thinking, …

OTHER
    rune -tools                        the tools the agent is offered
    rune -config [show|check|defaults]   inspect or validate rune.toml
    rune -config set <key> <value>      edit one global setting
    rune -config reset <key>            restore default/inheritance
    rune -config explain <key>          explain an effective setting
    rune -config format                 expand grouped settings without losing values
    rune -doctor                       check keys, sandbox and connectivity
    rune -host                         line-delimited JSON, for a GUI frontend
    rune -version | -help              show version or help screen

GLOBAL FLAGS (ANYWHERE)
    -project <dir>                     act on this workspace
    -db <path>                         use another engine database
    -read-only                         offer no tool that can change anything
    --no-color                         print without colour (or set NO_COLOR)
    -script <file>                     replay a recorded model script

KEYS, once running
    ctrl+o  session panel     ctrl+r  everything expanded    tab        permission mode
    ctrl+g  settings          ctrl+t  plan                   /undo   undo history
    ctrl+c  stop / quit       esc     close                  /help    commands
";

pub fn parse(argv: &[String]) -> Result<Args, String> {
    let mut out = Args {
        cmd: Cmd::Ui,
        project: None,
        db: None,
        read_only: false,
        no_color: false,
        script: None,
    };
    let mut words: Vec<&str> = Vec::new();
    let mut cmd: Option<&str> = None;
    let mut i = 0;

    while i < argv.len() {
        let a = argv[i].as_str();
        let value = |name: &str, i: &mut usize| -> Result<String, String> {
            *i += 1;
            argv.get(*i)
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        if cmd == Some("mcp") && a == "--" {
            words.extend(argv[i..].iter().map(String::as_str));
            break;
        }
        match a {
            "-project" | "--project" => {
                out.project = Some(PathBuf::from(value("-project", &mut i)?))
            }
            "-db" | "--db" => out.db = Some(PathBuf::from(value("-db", &mut i)?)),
            "-read-only" | "--read-only" => out.read_only = true,
            "-no-color" | "--no-color" => out.no_color = true,
            "-script" | "--script" => out.script = Some(PathBuf::from(value("-script", &mut i)?)),
            "-h" | "-help" | "--help" => {
                return Ok(Args {
                    cmd: Cmd::Help,
                    ..out
                })
            }
            "-v" | "-version" | "--version" => {
                return Ok(Args {
                    cmd: Cmd::Version,
                    ..out
                })
            }
            _ if a.starts_with('-') && cmd.is_none() => cmd = Some(a.trim_start_matches('-')),
            // A bare word before any command is the workspace: `rune ~/code/app`.
            _ if cmd.is_none() && out.project.is_none() && !a.starts_with('-') => {
                out.project = Some(PathBuf::from(a))
            }
            _ => words.push(a),
        }
        i += 1;
    }

    out.cmd = match cmd {
        None => Cmd::Ui,
        Some("p" | "prompt") => {
            let text = words.join(" ");
            if text.trim().is_empty() {
                return Err("-p needs something to ask".into());
            }
            Cmd::Once { text }
        }
        Some("host") => Cmd::Host,
        Some("detached-host") => Cmd::Detached {
            session: id(need(&words, 0, "detached-host needs a session ID")?)?,
        },
        Some("resume") => {
            let raw = need(
                &words,
                0,
                "-resume <session_id> (or run `rune -session` to browse interactively)",
            )?;
            Cmd::Resume { session: id(raw)? }
        }
        Some("session" | "sessions") => Cmd::Session(session(&words)?),
        Some("storage") => Cmd::Storage(storage(&words)?),
        Some("sandbox") => Cmd::Sandbox(sandbox(&words)?),
        Some("provider" | "providers") => Cmd::Provider(provider(&words, argv)?),
        Some("model" | "models") => Cmd::Model(model(&words)?),
        Some("set") => {
            let pair = words.join(" ");
            let (k, v) = pair
                .split_once('=')
                .ok_or("-set takes key=value, e.g. -set temperature=0.2")?;
            Cmd::Settings(SettingsCmd::Set {
                key: k.trim().to_string(),
                value: v.trim().to_string(),
            })
        }
        Some("settings") => Cmd::Settings(SettingsCmd::Show),
        Some("tools") => Cmd::Tools,
        Some(group @ ("skills" | "mcp")) => Cmd::Capabilities {
            group: group.into(),
            words: words.iter().map(|s| s.to_string()).collect(),
        },
        Some("config") => Cmd::Config {
            words: words.iter().map(|w| w.to_string()).collect(),
        },
        Some("doctor") => Cmd::Doctor,
        Some("upgrade-check") => Cmd::UpgradeCheck,
        Some(other) => return Err(format!("unknown command -{other}\n\n{USAGE}")),
    };
    Ok(out)
}

fn session(words: &[&str]) -> Result<Session, String> {
    match words.first().copied() {
        None => Ok(Session::List),
        Some("delete" | "rm") => Ok(Session::Delete {
            id: id(need(words, 1, "-session delete <id>")?)?,
        }),
        Some("rename") => {
            let id = id(need(words, 1, "-session rename <id> <title>")?)?;
            let title = words[2..].join(" ");
            if title.trim().is_empty() {
                return Err("-session rename <id> <title>".into());
            }
            Ok(Session::Rename { id, title })
        }
        Some("list") => Err("unknown: -session list (use `rune -session` to view sessions)".into()),
        Some(other) => Err(format!("unknown: -session {other}")),
    }
}

fn provider(words: &[&str], argv: &[String]) -> Result<Provider, String> {
    match words.first().copied() {
        None | Some("list") => Ok(Provider::List),
        Some("add") => {
            let name = need(words, 1, "-provider add <name> -key <key>")?.to_string();
            Ok(Provider::Add {
                preset: opt(argv, "-preset"),
                base_url: opt(argv, "-base").or_else(|| opt(argv, "-base-url")),
                key_name: opt(argv, "-key-name"),
                key: opt(argv, "-key"),
                model: opt(argv, "-model"),
                name,
            })
        }
        Some("remove" | "rm") => Ok(Provider::Remove {
            name: need(words, 1, "-provider remove <name>")?.to_string(),
        }),
        Some("timeout") => Ok(Provider::Timeout {
            name: need(words, 1, "-provider timeout <name> <seconds|default>")?.to_string(),
            seconds: match need(words, 2, "-provider timeout <name> <seconds|default>")? {
                "default" | "none" => None,
                n => Some(
                    n.parse()
                        .map_err(|_| "the timeout is a whole number of seconds".to_string())?,
                ),
            },
        }),
        Some("dryrun") => Ok(Provider::DryRun {
            name: need(words, 1, "-provider dryrun <name> [model]")?.to_string(),
            model: words.get(2).map(|s| s.to_string()),
        }),
        Some("keys") => Ok(Provider::Keys {
            name: need(words, 1, "-provider keys <name>")?.to_string(),
        }),
        Some("new-store") => Ok(Provider::NewStore {
            confirmed: words.get(1).is_some_and(|w| *w == "--yes"),
        }),
        Some("dropkey") => Ok(Provider::DropKey {
            name: need(words, 1, "-provider dropkey <name> <position>")?.to_string(),
            position: need(words, 2, "-provider dropkey <name> <position>")?
                .parse()
                .map_err(|_| "the position is the number `-provider keys` printed".to_string())?,
        }),
        Some("key") => Ok(Provider::Key {
            name: need(words, 1, "-provider key <name> <key>")?.to_string(),
            key: need(words, 2, "-provider key <name> <key>")?.to_string(),
        }),
        Some("default") => Ok(Provider::Default {
            name: need(words, 1, "-provider default <name>")?.to_string(),
        }),
        Some("test") => Ok(Provider::Test {
            name: need(words, 1, "-provider test <name>")?.to_string(),
            model: opt(argv, "-model"),
        }),
        Some("models") => Ok(Provider::Models {
            name: need(words, 1, "-provider models <name>")?.to_string(),
        }),
        Some(other) => Err(format!("unknown: -provider {other}")),
    }
}

fn storage(words: &[&str]) -> Result<Storage, String> {
    match words.first().copied() {
        None | Some("list") => Ok(Storage::List),
        Some("prune") => Ok(Storage::Prune),
        Some(other) => Err(format!("unknown: -storage {other}")),
    }
}

fn sandbox(words: &[&str]) -> Result<SandboxCmd, String> {
    match words {
        [] | ["list"] => Ok(SandboxCmd::List),
        ["reset"] => Ok(SandboxCmd::Reset),
        _ => Err(format!("unknown: -sandbox {}", words.join(" "))),
    }
}

fn model(words: &[&str]) -> Result<Model, String> {
    match words.first().copied() {
        None | Some("list") => Ok(Model::List),
        Some("set") => Ok(Model::Set {
            id: need(words, 1, "-model set <id>")?.to_string(),
        }),
        Some(other) => Err(format!("unknown: -model {other}")),
    }
}

/// A flag with a value, taken from the raw argv so it can appear anywhere.
///
/// `-key` is read here rather than from the command words because a key is a
/// value, not a word: `-provider add mimo -key sk-…` reads naturally, and
/// positional parsing would make the key's place in the line load-bearing.
fn opt(argv: &[String], flag: &str) -> Option<String> {
    argv.iter()
        .position(|a| a == flag)
        .and_then(|i| argv.get(i + 1))
        .cloned()
}

fn need<'a>(words: &'a [&'a str], at: usize, usage: &str) -> Result<&'a str, String> {
    words
        .get(at)
        .copied()
        .filter(|w| !w.is_empty())
        .ok_or_else(|| usage.to_string())
}

fn id(text: &str) -> Result<i64, String> {
    text.parse()
        .map_err(|_| format!("`{text}` is not a number"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_of(line: &str) -> Args {
        let argv: Vec<String> = line.split_whitespace().map(String::from).collect();
        parse(&argv).unwrap()
    }

    #[test]
    fn bare_rune_opens_the_ui_here() {
        let a = parse_of("");
        assert_eq!(a.cmd, Cmd::Ui);
        assert!(a.project.is_none());
    }

    #[test]
    fn a_bare_word_is_the_workspace() {
        assert_eq!(
            parse_of("/home/dev/app").project,
            Some(PathBuf::from("/home/dev/app"))
        );
    }

    #[test]
    fn a_prompt_keeps_all_of_its_words() {
        assert_eq!(
            parse_of("-p fix the login bug").cmd,
            Cmd::Once {
                text: "fix the login bug".into()
            }
        );
    }

    #[test]
    fn the_key_may_appear_anywhere_on_the_line() {
        let before = parse_of("-provider add mimo -key sk-abc -model mimo-v2.5-pro");
        let expected = Provider::Add {
            name: "mimo".into(),
            preset: None,
            base_url: None,
            key_name: None,
            key: Some("sk-abc".into()),
            model: Some("mimo-v2.5-pro".into()),
        };
        assert_eq!(before.cmd, Cmd::Provider(expected));
    }

    #[test]
    fn modifiers_work_with_any_command() {
        let a = parse_of("-session -project /tmp/x -read-only");
        assert_eq!(a.cmd, Cmd::Session(Session::List));
        assert_eq!(a.project, Some(PathBuf::from("/tmp/x")));
        assert!(a.read_only);
    }

    #[test]
    fn session_list_is_rejected_as_duplicate() {
        let argv: Vec<String> = vec!["-session".into(), "list".into()];
        let err = parse(&argv).unwrap_err();
        assert!(err.contains("unknown: -session list"));
    }

    #[test]
    fn storage_lists_by_default_and_prunes_on_request() {
        assert_eq!(parse_of("-storage").cmd, Cmd::Storage(Storage::List));
        assert_eq!(parse_of("-storage list").cmd, Cmd::Storage(Storage::List));
        assert_eq!(parse_of("-storage prune").cmd, Cmd::Storage(Storage::Prune));
        let argv: Vec<String> = vec!["-storage".into(), "wipe".into()];
        assert!(parse(&argv).unwrap_err().contains("unknown: -storage wipe"));
    }

    #[test]
    fn sandbox_lists_by_default_and_resets_on_request() {
        assert_eq!(parse_of("-sandbox").cmd, Cmd::Sandbox(SandboxCmd::List));
        assert_eq!(
            parse_of("-sandbox list").cmd,
            Cmd::Sandbox(SandboxCmd::List)
        );
        assert_eq!(
            parse_of("-sandbox reset").cmd,
            Cmd::Sandbox(SandboxCmd::Reset)
        );
        for wrong in [vec!["-sandbox", "wipe"], vec!["-sandbox", "reset", "now"]] {
            let argv: Vec<String> = wrong.iter().map(|w| w.to_string()).collect();
            assert!(parse(&argv).unwrap_err().contains("unknown: -sandbox"));
        }
    }

    #[test]
    fn plural_and_singular_names_both_work() {
        assert_eq!(parse_of("-providers").cmd, Cmd::Provider(Provider::List));
        // Setting a credential store aside takes the person's explicit yes.
        assert_eq!(
            parse_of("-provider new-store").cmd,
            Cmd::Provider(Provider::NewStore { confirmed: false })
        );
        assert_eq!(
            parse_of("-provider new-store --yes").cmd,
            Cmd::Provider(Provider::NewStore { confirmed: true })
        );
        assert_eq!(parse_of("-sessions").cmd, Cmd::Session(Session::List));
    }

    #[test]
    fn set_takes_a_pair_and_says_so_when_it_does_not() {
        assert_eq!(
            parse_of("-set temperature=0.2").cmd,
            Cmd::Settings(SettingsCmd::Set {
                key: "temperature".into(),
                value: "0.2".into()
            })
        );
        let argv: Vec<String> = vec!["-set".into(), "temperature".into()];
        assert!(parse(&argv).unwrap_err().contains("key=value"));
    }

    #[test]
    fn a_missing_argument_answers_with_the_usage_for_that_command() {
        let argv: Vec<String> = vec!["-session".into(), "delete".into()];
        assert!(parse(&argv).unwrap_err().contains("-session delete <id>"));
    }

    #[test]
    fn an_unknown_command_shows_the_whole_usage() {
        let argv: Vec<String> = vec!["-teleport".into()];
        let e = parse(&argv).unwrap_err();
        assert!(e.contains("unknown command"));
        assert!(e.contains("USAGE"));
    }

    #[test]
    fn rename_keeps_a_title_made_of_several_words() {
        assert_eq!(
            parse_of("-session rename 12 the login work").cmd,
            Cmd::Session(Session::Rename {
                id: 12,
                title: "the login work".into()
            })
        );
    }

    #[test]
    fn ids_that_are_not_numbers_are_refused_by_name() {
        let argv: Vec<String> = vec!["-resume".into(), "latest".into()];
        assert!(parse(&argv).unwrap_err().contains("latest"));
    }

    #[test]
    fn undo_is_only_an_interactive_slash_command() {
        for flag in ["-undo", "-checkpoint"] {
            for words in [
                vec![flag],
                vec![flag, "list"],
                vec![flag, "diff", "1"],
                vec![flag, "restore", "1"],
            ] {
                let argv = words.into_iter().map(String::from).collect::<Vec<_>>();
                assert!(parse(&argv).is_err(), "retired command accepted: {argv:?}");
            }
        }
        assert!(USAGE.contains("/undo"));
        assert!(!USAGE.contains("ctrl+u") && !USAGE.contains("rune -undo"));
    }

    #[test]
    fn resume_requires_session_id() {
        let argv: Vec<String> = vec!["-resume".into()];
        let err = parse(&argv).unwrap_err();
        assert!(err.contains("-resume <session_id>"));

        let ok = parse_of("-resume 67");
        assert_eq!(ok.cmd, Cmd::Resume { session: 67 });
    }
}
