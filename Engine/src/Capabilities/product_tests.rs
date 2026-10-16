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

#[test]
fn product_help_concurrent_first_starts_publish_one_complete_package() {
    let f = Fixture::new();
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
        assert_eq!(ensure(&home).unwrap(), root(&home), "{what}");
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
    settings.projects.clear();
    let personal = project.join(".rune/skills").join(NAME);
    std::fs::create_dir_all(&personal).unwrap();
    std::fs::write(
        personal.join("SKILL.md"),
        format!("---\nname: {NAME}\ndescription: Custom help\n---\nMy instructions\n"),
    )
    .unwrap();
    let (entries, _) = skills::discover(&settings, &f.home(), &project).unwrap();
    assert!(entries.iter().find(|e| e.id == ID).unwrap().shadowed);
    assert!(entries.iter().any(|e| e.source == "rune" && !e.shadowed));
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
        assert!(doc.contains(field.key), "undocumented {}", field.key);
        let shown = if row["default"].is_null() {
            "Inherit / unset".into()
        } else {
            row["default"].to_string()
        };
        assert!(
            doc.contains(&format!("| `{}` | `{shown}` |", field.key)),
            "wrong documented default {}",
            field.key
        );
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
