use super::*;
use crate::capabilities::{config, skills};
use serde_json::Value;
use std::collections::BTreeSet;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("product-help-{:032x}", rand::random::<u128>()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn home(&self) -> PathBuf {
        self.0.join("home")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn coverage() -> Value {
    serde_json::from_str(include_str!("product_coverage.json")).unwrap()
}

#[test]
fn context_and_background_help_teach_measured_usage_and_purposeful_checks() {
    let usage = document("data/usage.md").unwrap();
    assert!(usage.contains("provider's reports"));
    assert!(usage.contains("not a promise about the next request"));
    let compact = document("auxiliary/compact.md").unwrap();
    for fact in [
        "compact.max_batch_size",
        "actual context rejection",
        "does not convert",
        "no safe older group",
    ] {
        assert!(compact.contains(fact), "{fact}");
    }
    let terminal = document("tools/terminal-use.md").unwrap();
    for fact in [
        "for`/`while",
        "waiting command can itself move to background",
        "actual task work",
    ] {
        assert!(terminal.contains(fact), "{fact}");
    }
}

#[test]
fn mouse_copy_help_explains_release_caret_secret_and_terminal_owned_selection() {
    let guide = document("appearance/layout.md").unwrap();
    for fact in [
        "highlight",
        "release",
        "does not move the text cursor",
        "Masked secret",
        "Shift-drag",
        "ui.mouse = false",
        "tabs",
        "resize",
    ] {
        assert!(guide.contains(fact), "mouse copy help omits {fact}");
    }
}

/// Nothing but the revision folder and its own bookkeeping is ever left.
fn visible(parent: &Path) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(parent)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != LOCK && !n.starts_with(IN_USE))
        .collect();
    names.sort();
    names
}

#[test]
fn product_help_is_offline_isolated_idempotent_and_leaves_choices_alone() {
    let f = Fixture::new();
    let home = f.home();
    let path = ensure(&home).unwrap();
    let skill = path.join(NAME).join("SKILL.md");
    assert_eq!(
        std::fs::read_to_string(&skill).unwrap(),
        document("SKILL.md").unwrap()
    );
    let stamp = std::fs::metadata(&skill).unwrap().modified().unwrap();
    std::fs::write(home.join("rune.toml"), "[ui]\ntheme = \"cyan\"\n").unwrap();
    std::fs::create_dir_all(home.join("skills/personal")).unwrap();
    std::fs::write(home.join("skills/personal/note.md"), "my notes").unwrap();
    assert_eq!(ensure(&home).unwrap(), path);
    assert_eq!(std::fs::metadata(skill).unwrap().modified().unwrap(), stamp);
    assert_eq!(
        std::fs::read_to_string(home.join("rune.toml")).unwrap(),
        "[ui]\ntheme = \"cyan\"\n"
    );
    assert_eq!(
        std::fs::read_to_string(home.join("skills/personal/note.md")).unwrap(),
        "my notes"
    );
    assert!(!home.join("secrets.vault").exists());
    let other = f.0.join("other");
    assert_ne!(ensure(&other).unwrap(), path);
}

/// First starts that meet on a fresh home publish one whole package. The bug
/// of 2026-11-04 (CI run 41771425869): both saw no folder, one made it, and the
/// other removed it — the first one's lock and half-written package with it —
/// so that start failed with "No such file or directory". Many rounds, since
/// the race is a matter of timing.
#[test]
fn product_help_concurrent_first_starts_publish_one_complete_package() {
    for _ in 0..25 {
        let f = Fixture::new();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let home = f.home();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    parent(&home).unwrap()
                })
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), f.home().join(DIRECTORY));
        }
        let workers: Vec<_> = (0..6)
            .map(|_| {
                let home = f.home();
                std::thread::spawn(move || ensure(&home).unwrap())
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), root(&f.home()));
        }
        assert!(intact(&root(&f.home())));
        assert_eq!(
            visible(&f.home().join(DIRECTORY)),
            vec![revision().to_string()]
        );
    }
}

/// A file or a link that took the folder's name is replaced by the folder; a
/// link's target is never touched.
#[test]
fn product_help_replaces_a_file_that_took_its_folders_name() {
    let f = Fixture::new();
    let home = f.home();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(DIRECTORY), "not a folder").unwrap();
    assert!(plain_directory(&parent(&home).unwrap()));
}

/// The race above, made certain: a start that looked a moment ago and saw no
/// folder must not take away the folder another start has made since — with
/// its lock and the package it is writing inside it. Only what is not a
/// folder is ever removed in its place.
#[test]
fn product_help_never_removes_a_folder_another_start_made() {
    let f = Fixture::new();
    let folder = f.home().join(DIRECTORY);
    std::fs::create_dir_all(folder.join("in-progress")).unwrap();
    std::fs::write(folder.join("in-progress/part"), "another start's").unwrap();
    discard_unless_folder(&folder).unwrap();
    assert_eq!(
        std::fs::read_to_string(folder.join("in-progress/part")).unwrap(),
        "another start's"
    );
    // A file in its place goes; nothing there is not an error.
    let squatter = f.home().join("squatter");
    std::fs::write(&squatter, "x").unwrap();
    discard_unless_folder(&squatter).unwrap();
    assert!(!squatter.exists());
    discard_unless_folder(&squatter).unwrap();
}

/// Owner, 2026-10-16: one changed byte, an editor's swap file or a file
/// manager's note in the official folder refused every start of Rune. Any
/// difference is now replaced whole by the build's own copy, leaving nothing
/// of the damaged one behind and never touching what a link pointed at.
#[test]
fn product_help_any_difference_is_replaced_whole_and_leaves_no_trace() {
    let f = Fixture::new();
    let home = f.home();
    let outside = f.0.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("note"), "unchanged").unwrap();
    let package = || root(&home).join(NAME);
    type Damage<'a> = (&'static str, Box<dyn Fn() + 'a>);
    let damages: Vec<Damage> = vec![
        (
            "changed byte",
            Box::new(|| {
                let file = package().join("menus/tools.md");
                let mut text = std::fs::read(&file).unwrap();
                text.push(b'x');
                std::fs::write(file, text).unwrap();
            }),
        ),
        (
            "deleted file",
            Box::new(|| std::fs::remove_file(package().join("menus/tools.md")).unwrap()),
        ),
        (
            "extra file",
            Box::new(|| std::fs::write(package().join("extra.md"), "x").unwrap()),
        ),
        (
            "editor swap file",
            Box::new(|| std::fs::write(package().join("menus/.tools.md.swp"), "x").unwrap()),
        ),
        (
            "file manager note",
            Box::new(|| std::fs::write(package().join(".directory"), "x").unwrap()),
        ),
        (
            "hidden folder",
            Box::new(|| {
                std::fs::create_dir(package().join(".git")).unwrap();
                std::fs::write(package().join(".git/x"), "x").unwrap();
            }),
        ),
        (
            "file beside the package",
            Box::new(|| std::fs::write(root(&home).join("notes.txt"), "x").unwrap()),
        ),
        (
            "unreadable file",
            Box::new(|| {
                let file = package().join("menus/tools.md");
                crate::platform::set_file_mode(&file, 0o000).unwrap();
            }),
        ),
        (
            "a folder that lost its write bit, beside a read-only file",
            Box::new(|| {
                let menus = package().join("menus");
                crate::platform::set_file_mode(&menus.join("tools.md"), 0o444).unwrap();
                std::fs::write(menus.join("extra.md"), "x").unwrap();
                crate::platform::set_file_mode(&menus, 0o500).unwrap();
            }),
        ),
        (
            "file turned into a link",
            Box::new(|| {
                let file = package().join("SKILL.md");
                std::fs::remove_file(&file).unwrap();
                crate::platform::fixture::symlink(outside.join("note"), file).unwrap();
            }),
        ),
        (
            "revision turned into a link",
            Box::new(|| {
                std::fs::remove_dir_all(root(&home)).unwrap();
                crate::platform::fixture::symlink(&outside, root(&home)).unwrap();
            }),
        ),
        (
            "help folder turned into a link",
            Box::new(|| {
                std::fs::remove_dir_all(home.join(DIRECTORY)).unwrap();
                crate::platform::fixture::symlink(&outside, home.join(DIRECTORY)).unwrap();
            }),
        ),
        (
            "help folder turned into a file",
            Box::new(|| {
                std::fs::remove_dir_all(home.join(DIRECTORY)).unwrap();
                std::fs::write(home.join(DIRECTORY), "x").unwrap();
            }),
        ),
        (
            "a crash left a stage and a trash folder",
            Box::new(|| {
                std::fs::create_dir_all(home.join(DIRECTORY).join(format!("{STAGE}1/{NAME}")))
                    .unwrap();
                std::fs::create_dir_all(home.join(DIRECTORY).join(format!("{TRASH}2"))).unwrap();
                std::fs::write(package().join("extra.md"), "x").unwrap();
            }),
        ),
    ];
    for (what, damage) in damages {
        ensure(&home).unwrap();
        damage();
        assert_eq!(
            ensure(&home).unwrap_or_else(|error| panic!("{what}: {error}")),
            root(&home),
            "{what}"
        );
        assert!(intact(&root(&home)), "{what}");
        assert_eq!(
            visible(&home.join(DIRECTORY)),
            vec![revision().to_string()],
            "{what}"
        );
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            1,
            "{what}: what a link pointed at is untouched"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("note")).unwrap(),
            "unchanged"
        );
    }
}

#[test]
fn product_help_removes_revisions_no_running_process_holds() {
    let f = Fixture::new();
    let parent = ensure(&f.home()).unwrap().parent().unwrap().to_path_buf();
    let old = "a".repeat(64);
    let held = "b".repeat(64);
    for revision in [&old, &held] {
        std::fs::create_dir_all(parent.join(revision).join(NAME)).unwrap();
    }
    std::fs::create_dir(parent.join("not-a-revision")).unwrap();
    std::fs::create_dir_all(parent.join(format!("{STAGE}crashed/{NAME}"))).unwrap();
    std::fs::create_dir_all(parent.join(format!("{TRASH}crashed"))).unwrap();
    let holder = crate::lock::shared(
        &parent.join(format!("{IN_USE}{held}")),
        crate::lock::Wait::No,
    )
    .unwrap();
    prune(&parent).unwrap();
    assert!(!parent.join(&old).exists(), "an unused revision goes");
    assert!(!parent.join(format!("{IN_USE}{old}")).exists());
    assert!(
        parent.join(&held).exists(),
        "one a running build reads stays"
    );
    assert!(
        parent.join("not-a-revision").exists(),
        "only Rune's own names are removed"
    );
    assert!(!parent.join(format!("{STAGE}crashed")).exists());
    assert!(!parent.join(format!("{TRASH}crashed")).exists());
    assert!(
        intact(&root(&f.home())),
        "the current revision is never removed"
    );
    drop(holder);
    prune(&parent).unwrap();
    assert!(!parent.join(&held).exists());
}

#[test]
fn product_help_that_cannot_be_written_is_a_diagnostic_never_a_failed_discovery() {
    let f = Fixture::new();
    let project = f.0.join("project");
    std::fs::create_dir(&project).unwrap();
    let personal = f.home().join("skills/personal");
    std::fs::create_dir_all(&personal).unwrap();
    std::fs::write(
        personal.join("SKILL.md"),
        "---\nname: personal\ndescription: Mine\n---\nMy instructions\n",
    )
    .unwrap();
    let parent = f.home().join(DIRECTORY);
    std::fs::create_dir(&parent).unwrap();
    let read_only = crate::platform::fixture::Restricted::new(&parent, 0o500).unwrap();
    let settings = config::Settings {
        portable_user_skills: Some(false),
        ..Default::default()
    };
    let found = skills::discover(&settings, &f.home(), &project);
    drop(read_only);
    let (entries, diagnostics) = found.expect("discovery goes on without official help");
    assert!(entries.iter().any(|e| e.name == "personal"));
    assert!(!entries.iter().any(|e| e.id == ID));
    assert!(diagnostics
        .iter()
        .any(|d| d["source"] == "rune-product"
            && d["error"].as_str().unwrap().contains("unavailable")));
    let (entries, _) = skills::discover(&settings, &f.home(), &project).unwrap();
    assert!(
        entries.iter().any(|e| e.id == ID),
        "the next discovery provides it"
    );
}

#[test]
fn product_help_is_discoverable_with_stable_identity_and_user_controls() {
    let f = Fixture::new();
    let project = f.0.join("project");
    std::fs::create_dir(&project).unwrap();
    let mut settings = config::Settings {
        portable_user_skills: Some(false),
        ..Default::default()
    };
    let (entries, _) = skills::discover(&settings, &f.home(), &project).unwrap();
    let builtin = entries.iter().find(|e| e.id == ID).unwrap();
    assert_eq!(builtin.source, "rune-product");
    assert_eq!(builtin.scope, "product");
    assert!(!builtin.disabled && !builtin.shadowed);
    assert!(builtin
        .instructions(&settings)
        .unwrap()
        .contains("Rune user settings guide"));
    let mut project_settings = config::Project {
        root: project.clone(),
        ..Default::default()
    };
    project_settings.disabled_skills.push(ID.into());
    settings
        .projects
        .insert(config::project_key(&project), project_settings);
    let (entries, _) = skills::discover(&settings, &f.home(), &project).unwrap();
    assert!(entries.iter().find(|e| e.id == ID).unwrap().disabled);
}

fn custom_copy(root: &Path, text: &str) {
    let copy = root.join(NAME);
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::write(
        copy.join("SKILL.md"),
        format!("---\nname: {NAME}\ndescription: Custom help\n---\n{text}\n"),
    )
    .unwrap();
}

#[test]
fn no_other_package_takes_the_official_help_name() {
    let f = Fixture::new();
    let project = f.0.join("project");
    let mut settings = config::Settings {
        portable_user_skills: Some(false),
        ..Default::default()
    };
    // One arriving with a project (the Studio source is one), one in the
    // person's own skills: both found, neither offered in its place.
    custom_copy(&project.join(".rune/skills"), "Project instructions");
    custom_copy(&f.home().join("skills"), "Personal instructions");
    let (entries, _) = skills::discover(&settings, &f.home(), &project).unwrap();
    let builtin = entries.iter().find(|e| e.id == ID).unwrap();
    assert!(!builtin.shadowed && !builtin.disabled);
    let copies: Vec<_> = entries
        .iter()
        .filter(|e| e.name == NAME && e.id != ID)
        .collect();
    assert_eq!(copies.len(), 2);
    assert!(copies.iter().all(|e| e.shadowed));

    // Official help switched off does not hand its name to a copy.
    let mut choices = config::Project {
        root: project.clone(),
        ..Default::default()
    };
    choices.disabled_skills.push(ID.into());
    settings
        .projects
        .insert(config::project_key(&project), choices);
    let (entries, _) = skills::discover(&settings, &f.home(), &project).unwrap();
    assert!(entries
        .iter()
        .filter(|e| e.name == NAME)
        .all(|e| e.disabled || e.shadowed));

    // Nor can one be installed, or replace a copy already there (the Studio
    // source), in any scope.
    let source = f.0.join("source");
    custom_copy(&source, "Replacement");
    let fresh = f.0.join("fresh");
    for (destination, update) in [(&fresh, false), (&project.join(".rune/skills"), true)] {
        let error = skills::install(&source.join(NAME), destination, &settings, update)
            .expect_err("the official name is refused");
        assert!(matches!(error, Error::Invalid(_)), "{error}");
    }
    assert_eq!(std::fs::read_dir(&fresh).unwrap().count(), 0);
    let kept = std::fs::read_to_string(project.join(".rune/skills").join(NAME).join("SKILL.md"));
    assert!(kept.unwrap().contains("Project instructions"));
    assert_eq!(
        std::fs::read_dir(project.join(".rune/skills"))
            .unwrap()
            .count(),
        1,
        "nothing staged is left beside it"
    );
}

#[test]
fn product_help_covers_every_global_key_default_kind_knob_and_tool() {
    let map = coverage();
    let fields = map["fields"].as_object().unwrap();
    assert_eq!(fields.len(), crate::settings::configuration::FIELDS.len());
    for field in crate::settings::configuration::FIELDS {
        let row = &fields[field.key];
        assert_eq!(
            row["default"],
            serde_json::from_str::<Value>(field.default).unwrap(),
            "default {}",
            field.key
        );
        use crate::settings::configuration::Kind::*;
        let kind = match field.kind {
            Count(lo, hi) => format!("Count({lo},{hi})"),
            Bool => "Bool".into(),
            Text => "Text".into(),
            Duration => "Duration".into(),
            Size => "Size".into(),
            Money => "Money".into(),
            Model => "Model".into(),
            Generation => "Generation".into(),
            ToolNames => "ToolNames".into(),
            Capabilities => "Capabilities".into(),
            Connection => "Connection".into(),
        };
        assert_eq!(row["kind"], kind, "kind {}", field.key);
        let doc = document(row["document"].as_str().unwrap()).unwrap();
        // Rune's own constants became unsettings (owner, 2026-11-08): the help
        // documents what a person may change, so it never presents one of them
        // as a key with a default to set.
        if !crate::settings::configuration::is_fixed(field.key) {
            assert!(doc.contains(field.key), "undocumented {}", field.key);
        }
        let shown = if row["default"].is_null() {
            "Inherit / unset".into()
        } else {
            row["default"].to_string()
        };
        if !crate::settings::configuration::is_fixed(field.key) {
            assert!(
                doc.contains(&format!("| `{}` | `{shown}` |", field.key)),
                "wrong documented default {}",
                field.key
            );
        }
    }
    assert_eq!(
        map["knobs"],
        serde_json::json!(crate::settings::KNOBS
            .iter()
            .map(|k| k.key)
            .collect::<Vec<_>>())
    );
    for knob in crate::settings::KNOBS {
        assert!(document("generation/sampling.md")
            .unwrap()
            .contains(knob.key));
    }
    let names = crate::tools::Registry::default()
        .names()
        .iter()
        .map(|s| s.to_string())
        .collect::<BTreeSet<_>>();
    let mapped = map["tools"].as_object().unwrap();
    assert_eq!(names, mapped.keys().cloned().collect());
    for (name, path) in mapped {
        assert!(document(path.as_str().unwrap()).unwrap().contains(name));
    }
}

#[test]
fn product_help_nested_options_and_ui_inventory_stay_current() {
    let map = coverage();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    for (key, entry) in map["nested"].as_object().unwrap() {
        let (path, name) = key.split_once('#').unwrap();
        let source = std::fs::read_to_string(workspace.join(path)).unwrap();
        let body = source
            .split_once(&format!("pub struct {name} {{"))
            .unwrap()
            .1
            .split_once("\n}")
            .unwrap()
            .0;
        let props: Vec<_> = body
            .lines()
            .filter_map(|l| l.strip_prefix("    pub ")?.split_once(':').map(|(p, _)| p))
            .collect();
        assert_eq!(
            entry["properties"],
            serde_json::json!(props),
            "schema {name}"
        );
        let help = document(entry["document"].as_str().unwrap()).unwrap();
        for property in props {
            assert!(
                help.contains(property),
                "undocumented property {name}.{property}"
            );
        }
    }
    for (key, entry) in map["enums"].as_object().unwrap() {
        let (path, name) = key.split_once('#').unwrap();
        let source = std::fs::read_to_string(workspace.join(path)).unwrap();
        let body = source
            .split_once(&format!("pub enum {name} {{"))
            .unwrap()
            .1
            .split_once("\n}")
            .unwrap()
            .0;
        let mut variants = Vec::new();
        let mut properties = Vec::new();
        for line in body.lines() {
            if let Some(rest) = line
                .strip_prefix("    ")
                .filter(|r| r.starts_with(char::is_uppercase))
            {
                variants.push(
                    rest.split(|c: char| !c.is_ascii_alphabetic())
                        .next()
                        .unwrap(),
                );
            }
            if let Some((property, _)) = line
                .strip_prefix("        ")
                .and_then(|r| r.split_once(':'))
            {
                if property.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                    properties.push(property);
                }
            }
        }
        assert_eq!(
            entry["variants"],
            serde_json::json!(variants),
            "enum {name}"
        );
        assert_eq!(
            entry["properties"],
            serde_json::json!(properties),
            "enum properties {name}"
        );
        let text = document(entry["document"].as_str().unwrap())
            .unwrap()
            .to_ascii_lowercase();
        for property in properties {
            assert!(text.contains(property), "help for {name}.{property}");
        }
    }
    let source = std::fs::read_to_string(workspace.join("Cli/src/ui/settings.rs")).unwrap();
    for name in ["Page", "Field", "Kind"] {
        let body = source
            .split_once(&format!("pub enum {name} {{"))
            .unwrap()
            .1
            .split_once("\n}")
            .unwrap()
            .0;
        let variants: Vec<_> = body
            .lines()
            .filter_map(|l| {
                let l = l.strip_prefix("    ")?;
                if !l.starts_with(char::is_uppercase) {
                    return None;
                }
                Some(
                    l.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                        .next()
                        .unwrap(),
                )
            })
            .collect();
        let expected = if name == "Page" {
            map["pages"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>()
        } else {
            map["ui_variants"][name]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(
            variants
                .iter()
                .map(|v| v.to_string())
                .collect::<BTreeSet<_>>(),
            expected,
            "UI {name}"
        );
    }
    for path in map["pages"].as_object().unwrap().values() {
        assert!(document(path.as_str().unwrap()).is_some());
    }
    for preset in crate::settings::providers::PRESETS {
        assert!(document(&format!("connections/presets/{}.md", preset.name)).is_some());
    }
    for vendor in crate::web::ADAPTERS {
        let text = document(&format!("connections/vendors/{}.md", vendor.id)).unwrap();
        assert!(text.contains(vendor.title));
        assert!(
            text.contains(&format!("ceiling here: {}", vendor.max_results)),
            "vendor result count {}",
            vendor.id
        );
        assert!(
            text.contains(
                if vendor.serves(crate::settings::services::Service::Fetch) {
                    "search and page reading"
                } else {
                    "search only"
                }
            ),
            "vendor services {}",
            vendor.id
        );
    }
}

#[test]
fn reasoning_help_distinguishes_a_disable_request_from_the_upstream_effect() {
    let reasoning = document("models/reasoning.md").unwrap();
    let router = document("connections/presets/9router.md").unwrap();
    for required in [
        "not a guarantee",
        "Low",
        "Minimal",
        "Alt+C",
        "router/provider",
    ] {
        assert!(
            reasoning.contains(required),
            "reasoning help misses {required}"
        );
        assert!(router.contains(required), "router help misses {required}");
    }
    assert!(reasoning.contains("saved model overrides are not erased"));
    assert!(router.contains("omitting a choice can enable a positive default"));
}

#[test]
fn manual_shell_and_reference_link_help_cover_their_explicit_risks_and_behavior() {
    let inventory = coverage();
    for (feature, entry) in inventory["features"].as_object().unwrap() {
        let text = document(entry["document"].as_str().unwrap()).unwrap();
        for fact in entry["facts"].as_array().unwrap() {
            assert!(
                text.contains(fact.as_str().unwrap()),
                "{feature} omits {fact}"
            );
        }
    }
}

#[test]
fn terminal_help_explains_execution_choices_and_failed_handoff() {
    let text = document("tools/terminal-use.md").unwrap();
    for behavior in [
        "task's dependencies",
        "run_in_background: true",
        "same call",
        "moves automatically",
        "detection capability",
        "does not start",
        "it is stopped",
        "rather than repeatedly polling",
        "actual newlines, blank lines and indentation",
        "fills one visual row",
        "Each original newline appears as `↵`",
        "`…` appears only when a suffix cannot fit",
        "drag-copy restores original newlines",
        "terminal's own selection copies the visible display",
        "even while arguments stream or the command runs",
        "total omitted command and output lines",
        "engine's configured shell for the preview",
        "Bash/zsh use `$`, PowerShell uses `PS>`, and Cmd uses `>`",
        "Everything after it arrives with the task's notification",
        "saved output is synchronized to disk",
        "including a question without a final newline",
        "live screen at the end of the file is updated in place",
    ] {
        assert!(text.contains(behavior), "terminal help misses {behavior}");
    }
    let process = document("tools/process-control.md").unwrap();
    assert!(process.contains("the output the AI has not been given yet"));
    // One task by its id, and no word of a listing (owner, 2026-11-03: `all`
    // is gone, from every message).
    assert!(process.contains("Each call names one task by its id"));
    assert!(!process.contains("`all`"));
    assert!(process.contains("never instructions to the AI"));
    assert!(process.contains("types an answer into one that waits for input"));
    assert!(!text.contains("disk log remains empty"));
}

#[test]
fn footer_help_explains_status_competition_and_confirmation_lifetimes() {
    let text = document("appearance/rendering.md").unwrap();
    for behavior in [
        "8 seconds",
        "3 seconds of visible time",
        "Retry timing continues",
        "confirmations take priority",
        "including retry backoff",
        "another key disarms",
    ] {
        assert!(text.contains(behavior), "footer help misses {behavior}");
    }
}

#[test]
fn product_help_is_modular_linked_and_contains_no_developer_audit_details() {
    let f = Fixture::new();
    let root = ensure(&f.home()).unwrap().join(NAME);
    assert!(DOCUMENTS.len() > 60);
    let mut referenced = BTreeSet::from([PathBuf::from("SKILL.md")]);
    for (path, text) in DOCUMENTS {
        assert!(text.len() < 16000, "oversized help topic {path}");
        for forbidden in [
            "Source authorities",
            "Engine/src/",
            "Cli/src/",
            "write_guard",
            "RUNE_HANDOVER",
            "RUNE_PROCESS_TREE",
            "ROUTER_REFUSED",
            "x-9router",
            "secrets.vault",
            "master.key",
            "AES-256",
            "append-only",
            "sanitizer.jsonl",
        ] {
            assert!(
                !text.contains(forbidden),
                "internal detail in {path}: {forbidden}"
            );
        }
        for part in text.split("](").skip(1) {
            let target = part.split(')').next().unwrap().split('#').next().unwrap();
            if target.is_empty() || target.contains("://") {
                continue;
            }
            let linked = crate::jail::normalize(&Path::new(path).parent().unwrap().join(target));
            // A bundled topic is named by `/` on every OS.
            let name = linked
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            assert!(document(&name).is_some(), "broken link {path} → {target}");
            referenced.insert(linked);
        }
    }
    for (path, _) in DOCUMENTS {
        assert!(
            referenced.contains(Path::new(path)),
            "unlinked topic {path}"
        );
    }
    let settings = config::Settings::default();
    let bytes: usize = DOCUMENTS.iter().map(|(_, t)| t.len()).sum();
    assert!(bytes < settings.max_package_bytes);
    assert_eq!(
        skills::package_files(&root, &settings).unwrap().len(),
        DOCUMENTS.len()
    );
    skills::metadata(&root.join("SKILL.md"), settings.max_skill_bytes, true).unwrap();
}

#[test]
fn product_help_respects_the_capability_master_switch() {
    let f = Fixture::new();
    let project = f.0.join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir_all(f.home()).unwrap();
    std::fs::write(
        f.home().join("rune.toml"),
        "[capabilities]\nenabled = false\n",
    )
    .unwrap();
    let configuration = crate::settings::configuration::Configuration::load(&f.home()).unwrap();
    let cap = crate::capabilities::Capabilities::open(
        &configuration,
        &project,
        1,
        f.0.join("state.json"),
        crate::capabilities::mcp::Runtime::new().unwrap(),
        None,
    )
    .unwrap();
    assert!(cap.skill_entries(true).is_empty());
    assert!(!f.home().join(DIRECTORY).exists());
}

/// Owner, 2026-10-16: about a hundred numbers were glued to the word before
/// them ("to24h", "notch,1–30"). Code, links and value names stay as written.
#[test]
fn product_help_prose_keeps_numbers_apart_from_words() {
    for (path, text) in DOCUMENTS {
        let mut code = false;
        let mut prose = String::new();
        for part in text.split('`') {
            if !code {
                prose.push_str(part);
            }
            prose.push(' ');
            code = !code;
        }
        let chars: Vec<char> = prose.chars().collect();
        for (i, c) in chars.iter().enumerate() {
            if !c.is_ascii_digit() || i == 0 {
                continue;
            }
            let before = chars[i - 1];
            let glued_comma = before == ',' && i >= 2 && chars[i - 2].is_ascii_alphanumeric();
            if before.is_ascii_alphabetic() || glued_comma {
                let start = chars[..i]
                    .iter()
                    .rposition(|c| !(c.is_ascii_alphanumeric() || *c == ','))
                    .map_or(0, |p| p + 1);
                let word: String = chars[start..(i + 1).min(chars.len())].iter().collect();
                let previous = start.checked_sub(1).map(|p| chars[p]);
                let in_name = matches!(previous, Some('.' | '-' | '/' | '_' | '('))
                    || word.starts_with("ansi");
                assert!(in_name, "{path}: number glued in prose near {word:?}");
            }
        }
    }
}
