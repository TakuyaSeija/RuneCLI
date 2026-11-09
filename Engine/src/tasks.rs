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

/// One plan reminder's clocks: how many tool calls have passed since the last
/// reminder fired, and when it did. Held by the plan itself — a rewrite of the
/// plan restarts both clocks, because an agent that just showed it knows the
/// plan needs no reminder on top of the one it wrote.
#[derive(Default)]
struct Reminder {
    tools_since: u64,
    last_ms: Option<i64>,
}

pub struct Tasks {
    db: Arc<Mutex<Db>>,
    /// Read at every `set`, so the limits enforced are the ones in force now —
    /// the same values a hot-reloaded `rune.toml` gives every other tool.
    config: crate::settings::configuration::Shared,
    reminder: Mutex<Reminder>,
}

impl Tasks {
    pub fn new(db: Arc<Mutex<Db>>, config: crate::settings::configuration::Shared) -> Tasks {
        Tasks {
            db,
            config,
            reminder: Mutex::new(Reminder::default()),
        }
    }

    pub fn list(&self, session: i64) -> Result<Vec<Task>> {
        panic::lock(&self.db).tasks(session)
    }

    /// The plan to hand back, when a reminder is due: the tool cadence
    /// (`plan.reminder_tools`) or wall-clock time (`plan.reminder_seconds`),
    /// whichever comes first. Both clocks restart when the plan is rewritten
    /// or a reminder fires; the time trigger waits for its first fire, so a
    /// plan set seconds ago is not immediately asked for again. `None` when
    /// the plan is empty or both triggers are off — nothing to remind about,
    /// nothing to check against. `now_ms` is injected, so a test can walk the
    /// clock.
    pub fn reminder_due(&self, session: i64, now_ms: i64) -> Result<Option<String>> {
        let tools_every = self.config.number("plan.reminder_tools");
        let seconds_every =
            (self.config.duration("plan.reminder_seconds").as_millis() / 1000) as i64;
        if tools_every == 0 && seconds_every == 0 {
            return Ok(None);
        }
        let plan = self.list(session)?;
        if plan.is_empty() {
            return Ok(None);
        }
        let mut state = panic::lock(&self.reminder);
        state.tools_since += 1;
        let time_due = seconds_every > 0
            && state
                .last_ms
                .is_some_and(|at| now_ms.saturating_sub(at) >= seconds_every * 1000);
        let tools_due = tools_every > 0 && state.tools_since >= tools_every;
        if !(time_due || tools_due) {
            return Ok(None);
        }
        state.tools_since = 0;
        state.last_ms = Some(now_ms);
        Ok(Some(
            plan.iter()
                .map(|t| {
                    let mark = match t.status {
                        Status::Todo => "○",
                        Status::Doing => "◐",
                        Status::Done => "●",
                    };
                    format!("{mark} {}", t.text)
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ))
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
        let stored = db.tasks(session);
        // The plan was just rewritten: the agent showed it knows the plan, so
        // both reminder clocks restart — the wall clock from the write, the
        // tool cadence from zero.
        *panic::lock(&self.reminder) = Reminder {
            tools_since: 0,
            last_ms: Some(crate::store::now_ms()),
        };
        stored
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

    /// Owner, 2026-11-09: the plan comes back at the cadence the person set —
    /// the tool cadence, or wall-clock time, whichever first — and rewriting
    /// the plan restarts the clocks.
    #[test]
    fn a_plan_reminder_fires_on_its_cadence_and_restarts_when_rewritten() {
        let (tasks, id) = fixture();
        tasks
            .set(
                id,
                vec![
                    ("first".into(), Status::Doing),
                    ("second".into(), Status::Todo),
                ],
            )
            .unwrap();
        // The default tool cadence (plan.reminder_tools = 8): seven calls are
        // quiet, the eighth hands the plan back.
        for _ in 0..7 {
            assert_eq!(tasks.reminder_due(id, 1_000).unwrap(), None);
        }
        let due = tasks
            .reminder_due(id, 1_000)
            .unwrap()
            .expect("the eighth call fires");
        assert!(due.contains("◐ first"), "{due}");
        assert!(due.contains("○ second"), "{due}");
        // Both clocks restarted: the next calls are quiet again.
        for _ in 0..7 {
            assert_eq!(tasks.reminder_due(id, 1_001).unwrap(), None);
        }

        // The time trigger alone fires on elapsed wall-clock from the plan's
        // write, never before it.
        let home = std::env::temp_dir().join(format!("rune-tasks-time-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("rune.toml"),
            "[plan]\nreminder_tools = 0\nreminder_seconds = \"30s\"\n",
        )
        .unwrap();
        let config = crate::settings::configuration::Shared::load(&home).unwrap();
        let db = Db::memory().unwrap();
        let other = db.create_session("/tmp/p").unwrap().id;
        let timed = Tasks::new(Arc::new(Mutex::new(db)), config.clone());
        timed
            .set(other, vec![("only".into(), Status::Todo)])
            .unwrap();
        let base = crate::store::now_ms();
        assert_eq!(
            timed.reminder_due(other, base + 10_000).unwrap(),
            None,
            "no fire before the 30s have passed"
        );
        assert_eq!(timed.reminder_due(other, base + 20_000).unwrap(), None);
        let fired = timed
            .reminder_due(other, base + 50_000)
            .unwrap()
            .expect("30s elapsed since the plan was written");
        assert!(fired.contains("○ only"), "{fired}");
        assert_eq!(timed.reminder_due(other, base + 51_000).unwrap(), None);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// No plan, or both triggers off: nothing to remind about, ever.
    #[test]
    fn a_plan_reminder_stays_quiet_without_a_plan_or_with_both_triggers_off() {
        let (tasks, id) = fixture();
        assert_eq!(
            tasks.reminder_due(id, 1_000).unwrap(),
            None,
            "no plan was set"
        );
        tasks.set(id, vec![("a".into(), Status::Todo)]).unwrap();
        for _ in 0..7 {
            assert_eq!(tasks.reminder_due(id, 1_000).unwrap(), None);
        }

        let home = std::env::temp_dir().join(format!("rune-tasks-off-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("rune.toml"),
            "[plan]\nreminder_tools = 0\nreminder_seconds = \"0\"\n",
        )
        .unwrap();
        let config = crate::settings::configuration::Shared::load(&home).unwrap();
        let db = Db::memory().unwrap();
        let other = db.create_session("/tmp/p").unwrap().id;
        let off = Tasks::new(Arc::new(Mutex::new(db)), config.clone());
        off.set(other, vec![("a".into(), Status::Todo)]).unwrap();
        for at in 1..50 {
            assert_eq!(off.reminder_due(other, at * 10_000).unwrap(), None);
        }
        let _ = std::fs::remove_dir_all(&home);
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
