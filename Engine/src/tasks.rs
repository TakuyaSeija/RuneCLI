//! The plan: what the agent said it was going to do, held outside the context
//! window.
//!
//! A long turn overflows its context, and what gets squeezed out first is the
//! *intent* — what the work was for, and which parts are still outstanding. A
//! list the agent rewrites as it goes keeps that intent somewhere the model can
//! be handed back cheaply, and gives a frontend something honest to render
//! besides a spinner.
//!
//! # The one rule worth enforcing
//!
//! **At most one task is `doing` at a time.** This is not tidiness. An agent
//! that marks six things in progress has stopped sequencing its work and is
//! taking scattered swings at all of them; refusing the update is what keeps the
//! plan a plan. The engine enforces it rather than asking the model nicely,
//! because a rule stated only in a prompt is a rule that holds most of the time.
//!
//! This file is the concern; the rows live in [`crate::store::tasks`]. Same
//! split as [`crate::checkpoint`]: the wrapper owns the rules, the store owns
//! the SQL.

use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};
use crate::panic;
use crate::store::tasks::{Status, Task};
use crate::store::Db;

/// Longest a single task line may be. A plan is a list of intentions, not a
/// place to write the implementation.
pub const MAX_TEXT: usize = 200;

/// Most tasks one plan may hold. Past this the agent is not planning, it is
/// enumerating, and the whole list has to fit in a prompt.
pub const MAX_TASKS: usize = 50;

pub struct Tasks {
    db: Arc<Mutex<Db>>,
    /// Read at every `set`, so the limits enforced are the ones in force now —
    /// the same values a hot-reloaded `rune.toml` gives every other tool.
    config: crate::settings::configuration::Shared,
}

impl Tasks {
    pub fn new(db: Arc<Mutex<Db>>, config: crate::settings::configuration::Shared) -> Tasks {
        Tasks { db, config }
    }

    pub fn list(&self, session: i64) -> Result<Vec<Task>> {
        panic::lock(&self.db).tasks(session)
    }

    /// Replace the plan, after checking it is one.
    ///
    /// Returns the stored list, so the caller never has to guess what the engine
    /// kept — text is trimmed here, and a caller that assumed otherwise would
    /// show the user something the engine does not hold.
    pub fn set(&self, session: i64, items: Vec<(String, Status)>) -> Result<Vec<Task>> {
        let max_tasks = self.config.number("tasks.max_tasks") as usize;
        let max_text = self.config.number("tasks.max_chars_per_task") as usize;
        if items.len() > max_tasks {
            return Err(Error::invalid(format!(
                "a plan may hold at most {max_tasks} tasks"
            )));
        }

        let mut cleaned = Vec::with_capacity(items.len());
        for (text, status) in items {
            let text = text.trim().to_string();
            if text.is_empty() {
                return Err(Error::invalid("a task needs text"));
            }
            if text.chars().count() > max_text {
                return Err(Error::invalid(format!(
                    "a task must be at most {max_text} characters"
                )));
            }
            cleaned.push((text, status));
        }

        let doing = cleaned.iter().filter(|(_, s)| *s == Status::Doing).count();
        if doing > 1 {
            return Err(Error::invalid(format!(
                "only one task may be in progress at a time; {doing} were marked doing"
            )));
        }

        let db = panic::lock(&self.db);
        db.set_tasks(session, &cleaned)?;
        db.tasks(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Tasks, i64) {
        let db = Db::memory().unwrap();
        let id = db.create_session("/tmp/p").unwrap().id;
        (
            Tasks::new(
                Arc::new(Mutex::new(db)),
                crate::settings::configuration::Shared::new(
                    crate::settings::configuration::Configuration::default(),
                ),
            ),
            id,
        )
    }

    /// The limits are the ones in force when the plan is set, not the ones the
    /// session opened with: a `rune.toml` edit reaches the tool's description and
    /// its enforcement at the same moment, never one without the other.
    #[test]
    fn a_reloaded_limit_is_enforced_without_reopening_the_session() {
        let home = std::env::temp_dir().join(format!("rune-tasks-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("rune.toml"), "[tasks]\nmax_tasks = 2\n").unwrap();
        let config = crate::settings::configuration::Shared::load(&home).unwrap();
        let db = Db::memory().unwrap();
        let id = db.create_session("/tmp/p").unwrap().id;
        let tasks = Tasks::new(Arc::new(Mutex::new(db)), config.clone());
        let three = items(&[
            ("a", Status::Todo),
            ("b", Status::Todo),
            ("c", Status::Todo),
        ]);
        assert!(tasks.set(id, three.clone()).is_err());
        std::fs::write(home.join("rune.toml"), "[tasks]\nmax_tasks = 5\n").unwrap();
        config.reload().unwrap();
        assert_eq!(tasks.set(id, three).unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(&home);
    }

    fn items(list: &[(&str, Status)]) -> Vec<(String, Status)> {
        list.iter().map(|(t, s)| ((*t).to_string(), *s)).collect()
    }

    #[test]
    fn a_plan_round_trips_and_comes_back_as_it_was_stored() {
        let (t, s) = fixture();
        let stored = t
            .set(
                s,
                items(&[
                    ("  read the code  ", Status::Done),
                    ("fix it", Status::Doing),
                ]),
            )
            .unwrap();
        assert_eq!(
            stored[0].text, "read the code",
            "text is trimmed, and the caller is told so"
        );
        assert_eq!(stored[1].status, Status::Doing);
        assert_eq!(t.list(s).unwrap(), stored);
    }

    #[test]
    fn two_tasks_in_progress_is_refused() {
        let (t, s) = fixture();
        let why = t
            .set(s, items(&[("a", Status::Doing), ("b", Status::Doing)]))
            .unwrap_err()
            .to_string();
        assert!(why.contains("only one task"), "{why}");
        assert!(
            t.list(s).unwrap().is_empty(),
            "a refused plan must not be half-written"
        );
    }

    #[test]
    fn one_in_progress_among_many_is_the_normal_case() {
        let (t, s) = fixture();
        t.set(
            s,
            items(&[
                ("a", Status::Done),
                ("b", Status::Doing),
                ("c", Status::Todo),
                ("d", Status::Todo),
            ]),
        )
        .unwrap();
        assert_eq!(t.list(s).unwrap().len(), 4);
    }

    #[test]
    fn none_in_progress_is_allowed_because_a_finished_plan_has_none() {
        let (t, s) = fixture();
        t.set(s, items(&[("a", Status::Done), ("b", Status::Done)]))
            .unwrap();
        assert_eq!(t.list(s).unwrap().len(), 2);
    }

    #[test]
    fn empty_and_oversized_text_are_both_refused() {
        let (t, s) = fixture();
        assert!(t.set(s, items(&[("   ", Status::Todo)])).is_err());
        let long = "x".repeat(MAX_TEXT + 1);
        assert!(t.set(s, vec![(long, Status::Todo)]).is_err());
    }

    #[test]
    fn a_plan_longer_than_the_cap_is_refused_whole() {
        let (t, s) = fixture();
        let many: Vec<(String, Status)> = (0..MAX_TASKS + 1)
            .map(|n| (format!("task {n}"), Status::Todo))
            .collect();
        assert!(t.set(s, many).is_err());
        assert!(t.list(s).unwrap().is_empty());
    }

    #[test]
    fn clearing_the_plan_is_expressible() {
        let (t, s) = fixture();
        t.set(s, items(&[("a", Status::Todo)])).unwrap();
        assert!(t.set(s, Vec::new()).unwrap().is_empty());
    }
}
