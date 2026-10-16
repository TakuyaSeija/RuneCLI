//! The plan rows. One list per session, replaced whole.
//!
//! Ordering is the position column and nothing else: the agent sends the list it
//! wants, in the order it wants, and the store keeps exactly that. There is no
//! merge, because a merge would need row identity, and a list the agent rewrites
//! on every update has none.

use serde::{Deserialize, Serialize};

use super::Db;
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Not started.
    Todo,
    /// The one thing being worked on right now. At most one, ever.
    Doing,
    Done,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Todo => "todo",
            Status::Doing => "doing",
            Status::Done => "done",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub position: i64,
    pub text: String,
    pub status: Status,
}

impl Db {
    /// Replace a session's plan with `items`, in the order given. The
    /// session's own `tasks.json` is the single store for the plan.
    pub fn set_tasks(&self, session: i64, items: &[(String, Status)]) -> Result<()> {
        let tasks: Vec<Task> = items
            .iter()
            .enumerate()
            .map(|(i, (text, status))| Task {
                position: i as i64,
                text: text.clone(),
                status: *status,
            })
            .collect();
        self.jsonl.write_tasks(session, &tasks)?;
        Ok(())
    }

    pub fn tasks(&self, session: i64) -> Result<Vec<Task>> {
        self.jsonl.read_tasks(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(list: &[(&str, Status)]) -> Vec<(String, Status)> {
        list.iter().map(|(t, s)| ((*t).to_string(), *s)).collect()
    }

    #[test]
    fn a_plan_is_stored_and_read_back_in_order() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/p").unwrap();
        db.set_tasks(
            s.id,
            &items(&[
                ("read the code", Status::Done),
                ("write the fix", Status::Doing),
                ("test it", Status::Todo),
            ]),
        )
        .unwrap();

        let got = db.tasks(s.id).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].text, "read the code");
        assert_eq!(got[0].status, Status::Done);
        assert_eq!(got[1].status, Status::Doing);
        assert_eq!(got[2].position, 2);
    }

    #[test]
    fn setting_a_plan_replaces_the_previous_one_entirely() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/p").unwrap();
        db.set_tasks(s.id, &items(&[("a", Status::Todo), ("b", Status::Todo)]))
            .unwrap();
        db.set_tasks(s.id, &items(&[("c", Status::Doing)])).unwrap();

        let got = db.tasks(s.id).unwrap();
        assert_eq!(got.len(), 1, "the old rows must be gone, not merged");
        assert_eq!(got[0].text, "c");
    }

    #[test]
    fn an_empty_plan_clears_the_list() {
        let db = Db::memory().unwrap();
        let s = db.create_session("/tmp/p").unwrap();
        db.set_tasks(s.id, &items(&[("a", Status::Todo)])).unwrap();
        db.set_tasks(s.id, &[]).unwrap();
        assert!(db.tasks(s.id).unwrap().is_empty());
    }

    #[test]
    fn plans_belong_to_their_session_and_go_when_it_does() {
        let db = Db::memory().unwrap();
        let a = db.create_session("/tmp/p").unwrap();
        let b = db.create_session("/tmp/p").unwrap();
        db.set_tasks(a.id, &items(&[("mine", Status::Todo)]))
            .unwrap();
        db.set_tasks(b.id, &items(&[("theirs", Status::Todo)]))
            .unwrap();

        assert_eq!(db.tasks(a.id).unwrap()[0].text, "mine");
        db.delete_session(a.id).unwrap();
        assert!(db.tasks(a.id).unwrap().is_empty());
        assert_eq!(
            db.tasks(b.id).unwrap().len(),
            1,
            "the other session is untouched"
        );
    }
}
