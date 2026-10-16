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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sessions::Role;
    fn row(id: i64, role: Role, work: Option<Work>) -> Message {
        Message {
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
