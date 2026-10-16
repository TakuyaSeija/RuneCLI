//! Format 12 → 13 lays out classifier context selection and work provenance.
//! Old transcript bodies never change: only source/final metadata is added once,
//! with the original file kept by the upgrade plan. Runtime has no text heuristic.
use super::Plan;
use crate::error::Result;
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn apply(store: &Path, settings: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    super::rewrite_configuration(
        settings,
        backup,
        plan,
        crate::settings::configuration::laid_out,
    )?;
    promotions(store, backup, plan)?;
    super::rewrite_sessions(store, backup, plan, &["session.jsonl"], |_, old| {
        migrate(old)
    })
}

fn promotions(store: &Path, backup: &Path, plan: &mut Plan) -> Result<()> {
    let path = store.join("rune.db");
    if !path.is_file() {
        return Ok(());
    }
    let mut db = crate::store::connect(&path, std::time::Duration::from_secs(30))?;
    let present: bool = db.query_row(
        "SELECT count(*) > 0 FROM sqlite_master WHERE type='table' AND name='promotions'",
        [],
        |row| row.get(0),
    )?;
    if !present {
        return Ok(());
    }
    let rows: Vec<(String, String)> = {
        let mut query = db.prepare("SELECT token,journal FROM promotions")?;
        let rows = query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        rows
    };
    let mut updates = Vec::new();
    for (token, text) in rows {
        // A pending promotion that does not read is left as it was: it cannot
        // be recovered by any build, and the rest are upgraded.
        let Ok(mut journal) = serde_json::from_str::<Value>(&text) else {
            plan.skip(&path, format!("pending promotion {token} does not read"));
            continue;
        };
        let message = &mut journal["message"];
        if message.get("work").is_none_or(Value::is_null) {
            let Some(id) = message["id"].as_i64() else {
                plan.skip(&path, format!("pending promotion {token} names no message"));
                continue;
            };
            message["work"] = json!({"source":id,"finished":false});
            updates.push((token, serde_json::to_string(&journal)?));
        }
    }
    if updates.is_empty() {
        return Ok(());
    }
    let saved = backup.join("BeforeWorkMetadata.db");
    db.execute("VACUUM INTO ?1", [saved.to_string_lossy().as_ref()])?;
    plan.database = Some(saved);
    plan.save(backup)?;
    let transaction = db.transaction()?;
    for (token, journal) in updates {
        transaction.execute(
            "UPDATE promotions SET journal=?2 WHERE token=?1",
            rusqlite::params![token, journal],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

fn migrate(bytes: &[u8]) -> Vec<u8> {
    let mut source = None;
    let mut finished = true;
    super::rewrite_lines(bytes, |row| {
        let Some(id) = row["id"].as_i64() else {
            return false;
        };
        let role = row["role"].as_str().unwrap_or("");
        let mut added = false;
        if let Some(old) = row.get("work").filter(|v| v.is_object()) {
            source = old["source"].as_i64();
            finished = old["finished"].as_bool().unwrap_or(false);
        } else if role == "user" {
            if finished || source.is_none() {
                source = Some(id);
            }
            finished = false;
            row["work"] = json!({"source":source.unwrap(),"finished":false});
            added = true;
        } else if role == "assistant" && source.is_some() {
            let has_calls = row["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty());
            // Before work metadata existed a committed text-only answer is the
            // only durable final boundary; partial errors reopen it below.
            finished = !has_calls;
            row["work"] = json!({"source":source.unwrap(),"finished":finished});
            added = true;
        } else if role == "error" && source.is_some() {
            finished = false;
            row["work"] = json!({"source":source,"finished":false});
            added = true;
        }
        added
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_survives_failures_and_new_work_starts_only_after_a_final() {
        let old=b"{\"id\":1,\"role\":\"user\",\"content\":\"root\"}\n{\"id\":2,\"role\":\"assistant\",\"tool_calls\":[{}],\"content\":\"\"}\n{\"id\":3,\"role\":\"user\",\"content\":\"steer\"}\n{\"id\":4,\"role\":\"error\",\"content\":\"fail\"}\n{\"id\":5,\"role\":\"user\",\"content\":\"retry\"}\n{\"id\":6,\"role\":\"assistant\",\"content\":\"done\"}\n{\"id\":7,\"role\":\"user\",\"content\":\"next\"}\n";
        let next = migrate(old);
        let rows: Vec<Value> = std::str::from_utf8(&next)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(rows[2]["work"]["source"], 1);
        assert_eq!(rows[4]["work"]["source"], 1);
        assert_eq!(rows[6]["work"]["source"], 7);
        assert_eq!(rows[0]["content"], "root");
        assert_eq!(migrate(&next), next);
    }
    #[test]
    fn pending_promotion_gets_a_source_before_recovery_and_keeps_a_sqlite_snapshot() {
        let root = std::env::temp_dir().join(format!("upgrade13-promotion-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let backup = root.join("backup");
        std::fs::create_dir_all(&backup).unwrap();
        let db = rusqlite::Connection::open(root.join("rune.db")).unwrap();
        db.execute(
            "CREATE TABLE promotions(token TEXT PRIMARY KEY,journal TEXT)",
            [],
        )
        .unwrap();
        let old = "{\"message\":{\"id\":42,\"role\":\"user\",\"content\":\"draft\"}}";
        db.execute("INSERT INTO promotions VALUES('p',?1)", [old])
            .unwrap();
        drop(db);
        let mut plan = Plan::default();
        promotions(&root, &backup, &mut plan).unwrap();
        let db = rusqlite::Connection::open(root.join("rune.db")).unwrap();
        let updated: String = db
            .query_row("SELECT journal FROM promotions", [], |row| row.get(0))
            .unwrap();
        let row: Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(row["message"]["work"]["source"], 42);
        let saved = rusqlite::Connection::open(plan.database.unwrap()).unwrap();
        let original: String = saved
            .query_row("SELECT journal FROM promotions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(original, old);
        drop(db);
        drop(saved);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn upgrading_keeps_custom_limits_and_can_restore_every_original_byte() {
        let root = std::env::temp_dir().join(format!("upgrade13-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let backup = root.join("backup");
        std::fs::create_dir_all(home.join("projects/p/sessions/1")).unwrap();
        std::fs::create_dir_all(&backup).unwrap();
        let config = "[classifier]\nmax_evidence_size = \"512KiB\"\n";
        std::fs::write(home.join("rune.toml"), config).unwrap();
        let path = home.join("projects/p/sessions/1/session.jsonl");
        let old = b"{\"id\":1,\"role\":\"user\",\"content\":\"keep exactly\"}\n";
        std::fs::write(&path, old).unwrap();
        let mut plan = Plan::default();
        apply(&home, &home, &backup, &mut plan).unwrap();
        let now = crate::settings::configuration::Configuration::load(&home).unwrap();
        assert_eq!(now.number("classifier.max_evidence_size"), 512 * 1024);
        assert_eq!(
            now.get("classifier.context_tools")
                .as_array()
                .unwrap()
                .len(),
            6
        );
        assert_eq!(plan.files.len(), 1);
        assert_eq!(std::fs::read(&plan.files[0].1).unwrap(), old);
        for (live, saved) in &plan.files {
            crate::settings::secrets::atomic_write(live, &std::fs::read(saved).unwrap()).unwrap();
        }
        if let Some(saved) = &plan.config {
            std::fs::copy(saved, home.join("rune.toml")).unwrap();
        }
        assert_eq!(std::fs::read(&path).unwrap(), old);
        assert_eq!(
            std::fs::read_to_string(home.join("rune.toml")).unwrap(),
            config
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_damaged_line_is_preserved_not_rewritten_as_guessed_history() {
        let old = b"not json\n{\"id\":1,\"role\":\"user\",\"content\":\"kept\"}\npartial";
        let next = migrate(old);
        assert!(next.starts_with(b"not json\n"));
        assert!(next.ends_with(b"partial"));
    }
}
