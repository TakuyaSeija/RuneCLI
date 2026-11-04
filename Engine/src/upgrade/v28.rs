//! Format 27 → 28: `[manage_process]` becomes `[process_control]`.
//!
//! The tool that reported a background task's output was replaced by one that
//! only types and stops (owner, 2026-11-03): the output now arrives with the
//! task's own notification. The three keys itself are unchanged in meaning —
//! they bound how much of that log a notification may carry — so this step
//! moves the section and keeps every value, including a hand-written one.
//!
//! A home that already has `[process_control]` keeps its own: the newer key is
//! what this build reads, and two sections would be an ambiguity nobody asked
//! for. A `manage_process` that is not a table is left as it was written, for
//! the configuration's own check to name — one damaged item never fails a step,
//! nor keeps the classifier's tool lists from being renamed.
use super::Plan;
use crate::{Error, Result};
use std::path::Path;

pub(super) fn apply(settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    super::rewrite_configuration(settings, backup, plan, config)
}

/// `[manage_process]` moved, for this step and for judging an older step's
/// output by this build's keys.
pub(super) fn config(text: &str) -> Result<String> {
    let mut document: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| Error::invalid(format!("rune.toml: {e}")))?;
    // Damaged — not a table — it is left exactly where and as it was, for the
    // configuration's own check to say what is wrong with it; the lists below
    // are still renamed, since one damaged item never stops the rest of a step.
    let a_table = document
        .get("manage_process")
        .is_some_and(|older| older.as_table_like().is_some());
    let mut moved = false;
    if a_table {
        if let Some(older) = document.remove("manage_process") {
            if document.get("process_control").is_none() {
                document.insert("process_control", older);
            }
            moved = true;
        }
    }
    // A settings list naming a tool is matched exactly when it is applied, so a
    // former name left there is a setting that refuses the whole configuration.
    let renamed = rename_tool_names(&mut document);
    Ok(if moved || renamed {
        document.to_string()
    } else {
        text.into()
    })
}

/// Every former name of `process_control`, in the two settings lists that name
/// tools: the classifier's excluded set and its context set. Duplicates that a
/// rename creates are dropped, since the list is a set.
fn rename_tool_names(document: &mut toml_edit::DocumentMut) -> bool {
    const FORMER: [&str; 5] = [
        "manage_process",
        "manage_processes",
        "manage_process_list",
        "monitoring_bg",
        "terminal_send_input",
    ];
    let Some(classifier) = document
        .get_mut("classifier")
        .and_then(toml_edit::Item::as_table_like_mut)
    else {
        return false;
    };
    let mut changed = false;
    for key in ["excluded_tools", "context_tools"] {
        let Some(item) = classifier.get_mut(key) else {
            continue;
        };
        let Some(array) = item.as_array_mut() else {
            continue;
        };
        let mut renamed = toml_edit::Array::new();
        let mut seen: Vec<String> = Vec::new();
        for value in array.iter() {
            let Some(name) = value.as_str() else {
                renamed.push(value.clone());
                continue;
            };
            let name = if FORMER.contains(&name) {
                changed = true;
                "process_control"
            } else {
                name
            };
            if seen.iter().any(|had| had == name) {
                changed = true;
                continue;
            }
            seen.push(name.to_string());
            renamed.push(name);
        }
        *array = renamed;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_section_moves_with_every_value_it_held() {
        let text = "[manage_process]\nmax_output_chars = 1234\ntruncate_head_bytes = 2048\n\n[tools]\nenabled = true\n";
        let next = config(text).unwrap();
        assert!(!next.contains("manage_process"), "{next}");
        assert!(next.contains("[process_control]"), "{next}");
        for kept in [
            "max_output_chars = 1234",
            "truncate_head_bytes = 2048",
            "[tools]",
            "enabled = true",
        ] {
            assert!(next.contains(kept), "{kept}: {next}");
        }
        assert_eq!(config(&next).unwrap(), next, "done once");
        next.parse::<toml_edit::DocumentMut>().unwrap();
    }

    #[test]
    fn an_existing_newer_section_wins_and_the_old_one_goes() {
        let text = "[manage_process]\nmax_output_chars = 1234\n\n[process_control]\nmax_output_chars = 4321\n";
        let next = config(text).unwrap();
        assert!(!next.contains("manage_process"), "{next}");
        assert!(next.contains("max_output_chars = 4321"), "{next}");
        assert!(!next.contains("1234"), "{next}");
    }

    #[test]
    fn a_home_without_it_or_with_a_damaged_one_is_left_as_written() {
        for text in [
            "",
            "[ui]\nstatus_lifetime = '11s'\n",
            "manage_process = 7\n",
            "manage_process = \"off\"\n",
        ] {
            assert_eq!(config(text).unwrap(), text);
        }
    }

    /// A settings list naming a tool is matched exactly, so a former name left
    /// in one would refuse the whole configuration.
    #[test]
    fn the_classifier_lists_name_the_tool_that_exists_now() {
        let text = "[classifier]\nexcluded_tools = [\"manage_process\", \"inspect_file\", \"terminal_send_input\"]\ncontext_tools = [\"monitoring_bg\", \"process_control\"]\n";
        let next = config(text).unwrap();
        assert!(!next.contains("manage_process"), "{next}");
        assert!(!next.contains("monitoring_bg"), "{next}");
        assert!(!next.contains("terminal_send_input"), "{next}");
        let parsed: toml_edit::DocumentMut = next.parse().unwrap();
        let arrays = parsed["classifier"].as_table().unwrap();
        assert_eq!(
            arrays["excluded_tools"].to_string().trim(),
            r#"["process_control", "inspect_file"]"#
        );
        // A rename that meets the name already there leaves one copy, not two.
        assert_eq!(
            arrays["context_tools"].to_string().trim(),
            r#"["process_control"]"#
        );
        assert_eq!(config(&next).unwrap(), next, "done once");
    }

    /// One damaged item never stops the rest of a step: a `manage_process`
    /// that is not a table stays exactly as written for the configuration's
    /// check to name, and the lists naming a former tool are renamed anyway —
    /// left, they would refuse the configuration a second time.
    #[test]
    fn a_damaged_section_still_has_its_lists_renamed() {
        let text = "manage_process = 7\n\n[classifier]\nexcluded_tools = [\"terminal_send_input\", \"inspect_file\"]\n";
        let next = config(text).unwrap();
        assert!(next.contains("manage_process = 7"), "{next}");
        assert!(!next.contains("terminal_send_input"), "{next}");
        let parsed: toml_edit::DocumentMut = next.parse().unwrap();
        assert_eq!(parsed["manage_process"].as_integer(), Some(7), "{next}");
        assert_eq!(
            parsed["classifier"]["excluded_tools"].to_string().trim(),
            r#"["process_control", "inspect_file"]"#
        );
        assert_eq!(config(&next).unwrap(), next, "done once");
    }
}
