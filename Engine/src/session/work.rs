//! Work identity survives errors and host handover because it belongs to the
//! immutable conversation rows, not to an in-memory classifier cache.
use crate::store::sessions::{Message, Work};

pub(crate) fn current(history: &[Message]) -> Option<Work> {
    history.iter().rev().find_map(|row| row.work)
}

pub(crate) fn source(history: &[Message]) -> Option<i64> {
    current(history).map(|work| work.source)
}

/// A new request after a completed answer opens new work. A request after a
/// failure continues the still-unfinished source, so retrying does not lose it.
pub(crate) fn for_user(history: &[Message], id: i64) -> Work {
    match current(history) {
        Some(work) if !work.finished => Work {
            finished: false,
            ..work
        },
        _ => Work {
            source: id,
            finished: false,
        },
    }
}

/// Queue IDs come from the same monotonic ledger as message IDs. A final
/// written after acceptance still answers the older work; one already stored
/// when the person submitted means this is new work, even during a manual job.
/// No extra mutable scope journal or stored-shape change is needed.
pub(crate) fn for_queued(history: &[Message], id: i64, accepted: i64) -> Work {
    if let Some((row, work)) = history
        .iter()
        .rev()
        .find_map(|row| row.work.map(|work| (row, work)))
    {
        if work.finished && accepted < row.id {
            return Work {
                source: work.source,
                finished: false,
            };
        }
    }
    for_user(history, id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sessions::Role;
    fn row(id: i64, role: Role, work: Option<Work>) -> Message {
        Message {
            manual_shell: false,
            id,
            turn: id,
            role,
            content: String::new(),
            work,
            continuation: None,
            tool_call_id: None,
            tool_name: None,
            tool_calls: None,
            reasoning: None,
            thinking_tokens: None,
            media: Vec::new(),
            created_at: 0,
            context: None,
        }
    }
    #[test]
    fn failures_keep_the_source_but_a_final_answer_opens_the_next_work() {
        let mut history = vec![row(
            1,
            Role::User,
            Some(Work {
                source: 1,
                finished: false,
            }),
        )];
        history.push(row(2, Role::Error, None));
        assert_eq!(for_user(&history, 3).source, 1);
        history.push(row(
            4,
            Role::Assistant,
            Some(Work {
                source: 1,
                finished: true,
            }),
        ));
        assert_eq!(for_user(&history, 5).source, 5);
    }
    #[test]
    fn queued_work_uses_when_it_was_accepted_not_when_the_manual_job_finished() {
        let history = vec![
            row(
                1,
                Role::User,
                Some(Work {
                    source: 1,
                    finished: false,
                }),
            ),
            row(
                4,
                Role::Assistant,
                Some(Work {
                    source: 1,
                    finished: true,
                }),
            ),
        ];
        assert_eq!(
            for_queued(&history, 6, 3).source,
            1,
            "accepted before the final: still steering that work"
        );
        assert_eq!(
            for_queued(&history, 6, 5).source,
            6,
            "accepted after the final: new work, including during manual compaction"
        );
        let mut history = history;
        history.push(row(6, Role::User, Some(for_queued(&history, 6, 5))));
        assert_eq!(
            for_queued(&history, 8, 7).source,
            6,
            "the next queued message joins the new work"
        );
    }

    #[test]
    fn notifications_and_error_rows_never_become_a_user_source() {
        let history = vec![
            row(
                1,
                Role::User,
                Some(Work {
                    source: 1,
                    finished: false,
                }),
            ),
            row(2, Role::Context, None),
            row(3, Role::Error, None),
        ];
        assert_eq!(source(&history), Some(1));
    }
}
