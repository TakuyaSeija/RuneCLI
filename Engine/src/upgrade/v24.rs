//! Format 23 → 24: a background log includes its terminal's live suffix.
//!
//! Earlier files remain exact final-scrollback records: inventing the screen
//! they once showed would fabricate output. A running task's handed position
//! distinguishes its final prefix from its replaceable suffix. This additive
//! barrier prevents an older build opening the new layout; old journals, logs
//! and damaged records are deliberately left byte-for-byte unchanged.
use super::Plan;
use crate::Result;
use std::path::Path;

pub(super) fn apply(_store: &Path, _backup: &Path, _plan: &mut Plan) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn old_and_damaged_logs_and_journals_are_not_reinterpreted() {
        let store = super::super::tests::temp("background-v24");
        super::super::write_marker(&store, 23).unwrap();
        let folder = store.join("projects/example/sessions/1");
        std::fs::create_dir_all(folder.join("tools_result/terminal_use")).unwrap();
        let journal = folder.join("background.jsonl");
        let old = b"{\"event\":\"started\",\"task_id\":\"bg_old\",\"pid\":77,\"command\":\"echo old\",\"out_file\":\"/x/bg_old.txt\",\"started_at\":1}\n{damaged\n";
        std::fs::write(&journal, old).unwrap();
        let log = folder.join("tools_result/terminal_use/bg_old.txt");
        let text = b"old output\n\xff damaged bytes stay here\n";
        std::fs::write(&log, text).unwrap();
        super::super::prepare(&store, &store, &[], &mut |_| {}).unwrap();
        assert_eq!(
            super::super::marked(&store).unwrap(),
            Some(super::super::VERSION)
        );
        assert_eq!(std::fs::read(&journal).unwrap(), old);
        assert_eq!(std::fs::read(&log).unwrap(), text);
        let _ = std::fs::remove_dir_all(store);
    }
}
