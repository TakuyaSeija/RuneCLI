//! The provider model catalogue: `provider_models`, one row per model a
//! provider offers — what its models endpoint answered, merged with what the
//! person configured by hand. What a row holds is `settings::providers`'
//! business; where it is kept, and how, is this module's.
//!
//! # One door
//!
//! This is the only code that touches the table. It opens the database
//! exactly as the store does ([`super::connect`]: the busy timeout before
//! anything else, WAL only when the file is not already), because a second
//! Rune process starting alongside the first is refused instantly by a
//! connection with no busy handler. It does not run the store's start-up work:
//! the schema is the store's migration's, and recovering another process's
//! draft promotion from a settings read would be a write nobody asked for.
//! Every failure is returned — a catalogue that silently failed to save is a
//! model list that silently reverts on the next start.
//!
//! The database is named by its path, never derived from where the settings
//! live: in a split layout (a test, a second install) the settings directory
//! has no database, and deriving one there made a second `rune.db` beside
//! `rune.toml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};

use crate::error::{Error, Result};
use crate::settings::configuration::{Configuration, Shared};

/// Where the catalogue lives, and the configuration its connections read
/// their busy timeout from, live.
#[derive(Debug, Clone)]
pub struct Catalogue {
    db: PathBuf,
    config: Shared,
}

impl Default for Catalogue {
    fn default() -> Catalogue {
        Catalogue::nowhere()
    }
}

/// Whether the store's schema holds the catalogue's table yet.
fn laid(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'provider_models'",
        [],
        |row| row.get::<_, i64>(0),
    )? > 0)
}

/// One stored row: the provider, the model id, and the model as JSON.
pub type Row = (String, String, String);

impl Catalogue {
    /// The catalogue in the database at `db`.
    pub fn at(db: &Path, config: Shared) -> Catalogue {
        Catalogue {
            db: db.to_path_buf(),
            config,
        }
    }

    /// A catalogue with nowhere to live, for a configuration held only in
    /// memory: reading finds nothing, writing is refused.
    pub fn nowhere() -> Catalogue {
        Catalogue {
            db: PathBuf::new(),
            config: Shared::new(Configuration::default()),
        }
    }

    /// A connection, or `None` when there is nothing to read yet: no database,
    /// or one whose schema the store has not laid yet — another process is
    /// creating it this moment, and a window that started beside it read the
    /// half-made file, failed with "no such table" and closed (four windows
    /// opened at once on a new home). The first write to a database without
    /// the table opens it the way the store does — its schema included, made
    /// or waited for — so a row never lands in a table the store's migration
    /// did not make.
    fn open(&self, write: bool) -> Result<Option<Connection>> {
        if self.db.as_os_str().is_empty() {
            return if write {
                Err(Error::invalid("this catalogue has nowhere to be saved"))
            } else {
                Ok(None)
            };
        }
        let timeout = self.config.duration("storage.busy_timeout");
        if !self.db.exists() {
            if !write {
                return Ok(None);
            }
            drop(super::Db::open(&self.db, timeout)?);
        }
        let conn = super::connect(&self.db, timeout)?;
        if laid(&conn)? {
            return Ok(Some(conn));
        }
        if !write {
            return Ok(None);
        }
        drop(conn);
        drop(super::Db::open(&self.db, timeout)?);
        Ok(Some(super::connect(&self.db, timeout)?))
    }

    /// Every stored model as JSON, by provider, each provider's in id order.
    pub fn read(&self) -> Result<BTreeMap<String, Vec<String>>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let Some(conn) = self.open(false)? else {
            return Ok(out);
        };
        let mut statement =
            conn.prepare("SELECT provider, info FROM provider_models ORDER BY provider, id")?;
        let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))?;
        for row in rows {
            let (provider, info) = row?;
            out.entry(provider).or_default().push(info);
        }
        Ok(out)
    }

    /// Make `provider`'s rows exactly `models` (id, JSON), in one transaction.
    pub fn replace(&self, provider: &str, models: &[(String, String)]) -> Result<()> {
        self.write(|conn, at| {
            conn.execute(
                "DELETE FROM provider_models WHERE provider = ?1",
                params![provider],
            )?;
            for (id, info) in models {
                upsert(conn, provider, id, info, at)?;
            }
            Ok(())
        })
    }

    /// Insert or update `rows`, in one transaction.
    pub fn upsert(&self, rows: &[Row]) -> Result<()> {
        self.write(|conn, at| {
            for (provider, id, info) in rows {
                upsert(conn, provider, id, info, at)?;
            }
            Ok(())
        })
    }

    /// Forget every row of `provider`.
    pub fn forget_provider(&self, provider: &str) -> Result<()> {
        self.write(|conn, _| {
            conn.execute(
                "DELETE FROM provider_models WHERE provider = ?1",
                params![provider],
            )?;
            Ok(())
        })
    }

    /// Forget one model of `provider`.
    pub fn forget_model(&self, provider: &str, id: &str) -> Result<()> {
        self.write(|conn, _| {
            conn.execute(
                "DELETE FROM provider_models WHERE provider = ?1 AND id = ?2",
                params![provider, id],
            )?;
            Ok(())
        })
    }

    /// Run `change` in one immediate transaction: whole or not at all, and
    /// retried while another process holds the write lock.
    fn write(&self, change: impl Fn(&Connection, i64) -> Result<()>) -> Result<()> {
        let Some(mut conn) = self.open(true)? else {
            return Err(Error::invalid("this catalogue has nowhere to be saved"));
        };
        let at = super::now_ms();
        super::while_busy(|| {
            let transaction =
                conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            change(&transaction, at)?;
            transaction.commit()?;
            Ok(())
        })
    }
}

fn upsert(conn: &Connection, provider: &str, id: &str, info: &str, at: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO provider_models (provider, id, info, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (provider, id) DO UPDATE SET info = excluded.info, updated_at = excluded.updated_at",
        params![provider, id, info, at],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("rune-test")
            .join(format!("catalogue-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn catalogue(dir: &Path) -> Catalogue {
        Catalogue::at(&dir.join("rune.db"), Shared::new(Configuration::default()))
    }

    /// A database another process is still making — the file is there, the
    /// store's schema is not yet — holds nothing to read: four windows opened
    /// at once on a new home closed with "no such table". A write to it lays
    /// the schema the store's way first.
    #[test]
    fn a_database_whose_schema_is_still_being_made_reads_as_empty() {
        let dir = home("half-made");
        let db = dir.join("rune.db");
        drop(rusqlite::Connection::open(&db).unwrap());
        assert!(db.exists());
        let catalogue = catalogue(&dir);
        assert!(catalogue.read().unwrap().is_empty());
        catalogue
            .replace("p", &[("m".into(), "{\"id\":\"m\"}".into())])
            .unwrap();
        assert_eq!(
            catalogue.read().unwrap()["p"],
            vec!["{\"id\":\"m\"}".to_string()]
        );
        let version: i64 = crate::store::Db::open(&db, std::time::Duration::from_secs(1))
            .unwrap()
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, crate::store::SCHEMA_VERSION);
    }

    /// Reading creates nothing; the first write creates the database the way
    /// the store does, so the store opens it afterwards as its own.
    #[test]
    fn reading_creates_nothing_and_the_first_write_makes_the_stores_database() {
        let dir = home("first");
        let catalogue = catalogue(&dir);
        assert!(catalogue.read().unwrap().is_empty());
        assert!(!dir.join("rune.db").exists(), "a read made a database");

        catalogue
            .replace("p", &[("m".into(), "{\"id\":\"m\"}".into())])
            .unwrap();
        let db = crate::store::Db::open(&dir.join("rune.db"), std::time::Duration::from_secs(1))
            .unwrap();
        let version: i64 = db
            .conn()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, crate::store::SCHEMA_VERSION);
        assert_eq!(
            catalogue.read().unwrap()["p"],
            vec!["{\"id\":\"m\"}".to_string()]
        );
    }

    /// Replace is exact for its provider and leaves the others alone; the
    /// forgets remove exactly what they name.
    #[test]
    fn each_change_touches_only_what_it_names() {
        let dir = home("changes");
        let catalogue = catalogue(&dir);
        catalogue
            .upsert(&[
                ("a".into(), "1".into(), "a1".into()),
                ("a".into(), "2".into(), "a2".into()),
                ("b".into(), "1".into(), "b1".into()),
            ])
            .unwrap();
        catalogue
            .replace("a", &[("3".into(), "a3".into())])
            .unwrap();
        let rows = catalogue.read().unwrap();
        assert_eq!(rows["a"], vec!["a3"]);
        assert_eq!(rows["b"], vec!["b1"]);

        catalogue
            .upsert(&[("b".into(), "2".into(), "b2".into())])
            .unwrap();
        catalogue.forget_model("b", "1").unwrap();
        catalogue.forget_provider("a").unwrap();
        let rows = catalogue.read().unwrap();
        assert!(!rows.contains_key("a"));
        assert_eq!(rows["b"], vec!["b2"]);
    }

    /// A failure is the caller's to see: an in-memory catalogue refuses a
    /// write instead of pretending it saved, and a database that cannot be
    /// read is an error, never an empty catalogue.
    #[test]
    fn failures_are_reported_never_swallowed() {
        assert!(Catalogue::nowhere().read().unwrap().is_empty());
        assert!(Catalogue::nowhere()
            .replace("p", &[("m".into(), "{}".into())])
            .is_err());

        let dir = home("broken");
        std::fs::write(dir.join("rune.db"), b"this is not a database").unwrap();
        assert!(catalogue(&dir).read().is_err());
        assert!(catalogue(&dir).forget_provider("p").is_err());
    }
}
