//! Main-chat previews (tools and sent user messages) and permission readers are
//! different surfaces. Previews fold into four rows and expand on a click; readers keep their
//! own larger, scrollable card. A fold footer never promises wheel scrolling.

pub(super) const MAIN_PREVIEW_ROWS: usize = 4;

/// Reading-card density remains independent from main-chat previews and from
/// the composer's separately specified multiline budget.
pub(super) fn compact_rows(previous: usize) -> usize {
    (previous.saturating_mul(4) / 5).max(1)
}

pub(super) fn preview_rows(open: bool) -> usize {
    if open {
        usize::MAX
    } else {
        MAIN_PREVIEW_ROWS
    }
}

/// Four rows are a MAXIMUM, not reserved space. This must be a continuous
/// prefix of notice + output: a footer below it means the unseen suffix is
/// below, never that older output was silently replaced by the tail.
pub(super) fn terminal_preview(notices: &[String], output: &[String]) -> (Vec<String>, usize) {
    let shown: Vec<String> = notices
        .iter()
        .chain(output)
        .take(MAIN_PREVIEW_ROWS)
        .cloned()
        .collect();
    let omitted = notices.len() + output.len() - shown.len();
    (shown, omitted)
}

/// Pinned controls take priority on a short permission/question reader.
pub(super) fn card_rows(previous: usize, minimum: usize, available: usize) -> usize {
    compact_rows(previous).max(minimum).min(available)
}

/// Tool output is not deleted, and this footer is not a viewport boundary. It
/// sits AFTER the preview and tells the person how to reveal the omitted rows.
/// `lines cut` names text lines left out of this preview; ↑/↓ rows belong only to actual scrolling.
pub(super) fn cut_lines(count: usize) -> String {
    format!("… {count} lines cut. Click to open all")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notices_and_output_share_one_budget_and_omission_count_is_exact() {
        for notices in [0, 1, 4, 30] {
            for output in [0, 1, 4, 50] {
                let n: Vec<_> = (0..notices).map(|i| format!("notice {i}")).collect();
                let o: Vec<_> = (0..output).map(|i| format!("output {i}")).collect();
                let (shown, omitted) = terminal_preview(&n, &o);
                assert!(shown.len() <= MAIN_PREVIEW_ROWS);
                assert_eq!(shown.len() + omitted, notices + output);
                let all: Vec<String> = n.iter().chain(&o).cloned().collect();
                assert_eq!(shown, all[..all.len().min(MAIN_PREVIEW_ROWS)]);
            }
        }
    }

    #[test]
    fn main_preview_budget_never_depends_on_mode_lifecycle_or_card_density() {
        assert_eq!(preview_rows(false), 4);
        assert_eq!(preview_rows(true), usize::MAX);
        assert_eq!(card_rows(24, 8, 40), 19);
        assert_eq!(card_rows(4, 6, 6), 6);
        let fold = cut_lines(17);
        assert!(fold.contains("17 lines cut") && fold.contains("Click to open all"));
        assert!(!fold.contains("hidden") && !fold.contains('↑') && !fold.contains('↓'));
    }
}
