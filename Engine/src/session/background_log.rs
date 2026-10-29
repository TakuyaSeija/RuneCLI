//! A log is readable while the terminal still owns its last rows. Keep one
//! immutable prefix (rows that scrolled off) and replace only the live suffix,
//! through the same AVT screen as the foreground. No text matching can decide
//! that a later, equal line was already written: byte boundaries do that.
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// Bytes of final scrollback, never rewritten by an ordinary update.
    pub stable: u64,
    /// End of the published file, including the current screen.
    pub end: u64,
}

impl Position {
    /// The caller serializes publication and process readers with the task's
    /// output lock. Write precedes truncation; metadata commits only after a
    /// successful flush. This is not a transaction against power loss mid-write:
    /// durable wait publication is established by `sync` before its wake.
    pub fn publish(
        &mut self,
        file: &mut File,
        drained: &str,
        screen: &str,
        limit: usize,
    ) -> Result<(), String> {
        if self.stable > limit as u64 {
            return Err(limit_note(limit));
        }
        let result = (|| -> std::io::Result<bool> {
            file.seek(SeekFrom::Start(self.stable))?;
            let room = limit.saturating_sub(self.stable as usize);
            let kept = prefix(drained, room);
            file.write_all(kept.as_bytes())?;
            let stable = self.stable + kept.len() as u64;
            let complete = kept.len() == drained.len();
            let room = limit.saturating_sub(stable as usize);
            let kept = if complete { prefix(screen, room) } else { "" };
            file.write_all(kept.as_bytes())?;
            let end = stable + kept.len() as u64;
            file.set_len(end)?;
            file.flush()?;
            *self = Self { stable, end };
            Ok(complete && kept.len() == screen.len())
        })();
        match result {
            Ok(true) => Ok(()),
            Ok(false) => Err(limit_note(limit)),
            Err(error) => Err(format!(
                "Output capture failed ({error}); process terminated."
            )),
        }
    }

    /// An observed wait is not announced until its question has reached the
    /// filesystem, independently of conversation history or a live monitor.
    pub fn sync(file: &File) -> Result<(), String> {
        file.sync_data()
            .map_err(|error| format!("Output capture failed ({error}); process terminated."))
    }
}

fn prefix(text: &str, room: usize) -> &str {
    let mut end = room.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn limit_note(limit: usize) -> String {
    format!(
        "Output capture exceeded {} bytes; process terminated. Log contains the captured prefix only.",
        limit
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_screen::Screen;

    struct Fixture {
        path: std::path::PathBuf,
        file: File,
        position: Position,
    }
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "background-log-{:032x}.txt",
                rand::random::<u128>()
            ));
            let file = File::create(&path).unwrap();
            Self {
                path,
                file,
                position: Position::default(),
            }
        }
        fn update(&mut self, screen: &mut Screen) {
            self.position
                .publish(
                    &mut self.file,
                    &screen.drain(),
                    &screen.shown().text(),
                    1 << 20,
                )
                .unwrap();
        }
        fn text(&self) -> String {
            std::fs::read_to_string(&self.path).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn question_is_on_disk_before_any_row_scrolls_and_is_never_appended_twice() {
        let mut f = Fixture::new();
        let mut screen = Screen::log(3, 40);
        screen.feed("tes\r\nQuestion? ");
        f.update(&mut screen);
        Position::sync(&f.file).unwrap();
        assert_eq!(f.text(), "tes\nQuestion?");
        assert_eq!(f.position.stable, 0);
        screen.feed("yes\r\nanswer:yes\r\nnext\r\n");
        f.update(&mut screen);
        assert_eq!(f.text(), "tes\nQuestion? yes\nanswer:yes\nnext");
        let end = f.position.end;
        for _ in 0..10 {
            f.update(&mut screen);
        }
        assert_eq!(f.position.end, end);
        assert_eq!(f.text().matches("tes").count(), 1);
        assert_eq!(f.text().matches("Question?").count(), 1);
    }

    #[test]
    fn redraw_shrink_blank_rows_utf8_and_soft_wrap_keep_the_same_screen() {
        let mut f = Fixture::new();
        let mut screen = Screen::log(4, 8);
        let mut expected = Screen::log(4, 8);
        for text in [
            "one\r\n\r\nlong value",
            "\r\x1b[2K短",
            "\r\nabcdefghijklmno",
            "\x1b[1A\r\x1b[2Kchanged",
        ] {
            screen.feed(text);
            expected.feed(text);
            f.update(&mut screen);
            assert_eq!(f.text(), expected.text());
        }
    }

    #[test]
    fn every_character_split_preserves_history_and_terminal_semantics() {
        let source = "same\r\nsame\r\n\r\n界é\r\nprogress 50%\r\x1b[2Kprogress 100%\r\nabcdefghi\x1b[1A\r\x1b[2Krewritten\x1b[2J\x1b[Hnew\r\n";
        for (rows, cols) in [(2, 3), (4, 8), (24, 80)] {
            let mut f = Fixture::new();
            let mut screen = Screen::log(rows, cols);
            let mut oracle = Screen::log(rows, cols);
            for ch in source.chars() {
                let text = ch.to_string();
                screen.feed(&text);
                oracle.feed(&text);
                f.update(&mut screen);
                let wanted = oracle.text();
                assert_eq!(
                    f.text().trim_end_matches('\n'),
                    wanted,
                    "{rows}x{cols} after {text:?}"
                );
            }
        }
    }

    #[test]
    fn equal_lines_remain_distinct_after_many_scrolls_and_resizes() {
        let mut f = Fixture::new();
        let mut screen = Screen::log(3, 8);
        let mut oracle = Screen::log(3, 8);
        for n in 0..500 {
            if n % 17 == 0 {
                let cols = if n % 2 == 0 { 5 } else { 12 };
                screen.resize(3, cols);
                oracle.resize(3, cols);
            }
            let line = format!("same {n}\r\n");
            screen.feed(&line);
            oracle.feed(&line);
            f.update(&mut screen);
            assert_eq!(f.text().trim_end_matches('\n'), oracle.text());
        }
        assert_eq!(f.text().matches("same ").count(), 500);
    }

    #[test]
    fn cap_applies_to_live_suffix_without_scrolling_and_never_splits_utf8() {
        let mut f = Fixture::new();
        let err = f.position.publish(&mut f.file, "", "abé界", 6).unwrap_err();
        assert!(err.contains("exceeded 6 bytes"));
        assert_eq!(f.text(), "abé");
        assert!(f.position.end <= 6);
    }

    #[test]
    fn a_failed_write_does_not_advance_the_published_boundaries() {
        let mut f = Fixture::new();
        f.position
            .publish(&mut f.file, "before\n", "Question?", 100)
            .unwrap();
        let before = f.position;
        f.file = File::open(&f.path).unwrap();
        let error = f
            .position
            .publish(&mut f.file, "more\n", "new", 100)
            .unwrap_err();
        assert!(error.contains("capture failed"));
        assert_eq!(f.position, before);
        assert_eq!(f.text(), "before\nQuestion?");
    }

    #[test]
    fn repeated_redraws_do_not_consume_the_limit_again() {
        let mut f = Fixture::new();
        for n in 0..10_000 {
            f.position
                .publish(&mut f.file, "", &format!("{n:04}%"), 5)
                .unwrap();
        }
        assert_eq!(f.text(), "9999%");
        assert_eq!(f.position, Position { stable: 0, end: 5 });
    }

    #[test]
    fn released_position_keeps_the_same_prefix_without_guessing_text() {
        let mut f = Fixture::new();
        let mut screen = Screen::log(3, 10);
        screen.feed("first\r\nsecond\r\nthird\r\nQuestion?");
        f.update(&mut screen);
        let handed = serde_json::to_vec(&f.position).unwrap();
        let shown = screen.shown();
        let mut next = Screen::resume(3, 10, &shown);
        f.position = serde_json::from_slice(&handed).unwrap();
        f.file = std::fs::OpenOptions::new()
            .write(true)
            .open(&f.path)
            .unwrap();
        f.update(&mut next);
        assert_eq!(f.text(), "first\nsecond\nthird\nQuestion?");
        next.feed("yes\r\nfinal");
        f.update(&mut next);
        assert_eq!(f.text(), "first\nsecond\nthird\nQuestion?yes\nfinal");
        assert_eq!(f.text().matches("Question?").count(), 1);
    }
}
