//! What a terminal multiplexer between Rune and the screen lets through.
//!
//! Inside tmux, `TERM` describes tmux's own screen (`tmux-256color`), not the
//! terminal a person reads it on. tmux translates every colour for each
//! terminal attached to it, by what that terminal says it is: one attached as
//! `xterm` (eight colours) — `docker exec -it` without `-e TERM` does exactly
//! that — gets Rune's amber 256-colour index 172 as plain red (`31`;
//! measured, tmux 3.3a, SeaDrive console 2026-10-21), and every accent of the
//! theme with it. Told the truth, Rune picks the nearest of sixteen itself:
//! amber stays a yellow.
//!
//! So the terminals attached to this session are asked of tmux, and the
//! fewest colours any of them can show bound what Rune sends: a frame is drawn
//! once for every window watching it. Each is read by the rule Rune reads its
//! own `TERM` with (`ansi::depth`), plus tmux's `RGB` feature. Nothing is
//! assumed of a tmux that does not answer (too old, gone, no client attached):
//! Rune's own `TERM` decides alone, as before.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::ansi::Depth;

/// How long tmux may take to answer before it is not asked again: colour is
/// decided once, as the window starts. (Unit tests decide no colour from the
/// environment, `ansi::depth`, so nothing asks tmux there.)
#[cfg_attr(test, allow(dead_code))]
const ANSWER_WITHIN: Duration = Duration::from_millis(500);

/// The most colours every terminal attached to this tmux session can show;
/// `None` outside tmux, or when tmux could not say.
#[cfg_attr(test, allow(dead_code))]
pub fn ceiling() -> Option<Depth> {
    if std::env::var("TMUX").map_or(true, |tmux| tmux.is_empty()) {
        return None;
    }
    let pane = std::env::var("TMUX_PANE").ok()?;
    let answer = ask_tmux(&pane)?;
    ceiling_of(&answer)
}

/// One `tmux` run: this pane's session, then every attached client as
/// `session|terminal name|features`.
#[cfg_attr(test, allow(dead_code))]
fn ask_tmux(pane: &str) -> Option<String> {
    let mut child = Command::new("tmux")
        .args(["display-message", "-p", "-t", pane, "#{session_name}", ";"])
        .args([
            "list-clients",
            "-F",
            "#{client_session}|#{client_termname}|#{client_termfeatures}",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + ANSWER_WITHIN;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    let mut answer = String::new();
    child.stdout.take()?.read_to_string(&mut answer).ok()?;
    Some(answer)
}

/// The ceiling `answer` gives ([`ask_tmux`]'s shape): the least of what the
/// clients of the first line's session can show.
fn ceiling_of(answer: &str) -> Option<Depth> {
    let mut lines = answer.lines();
    let session = lines.next()?.trim();
    lines
        .filter_map(|line| {
            let mut fields = line.splitn(3, '|');
            if fields.next()? != session {
                return None;
            }
            let name = fields.next().unwrap_or("").to_ascii_lowercase();
            client_depth(&name, fields.next().unwrap_or(""))
        })
        .min_by_key(|depth| rank(*depth))
}

/// What one attached terminal can show: tmux's own word for full colour
/// (`RGB`, set by its terminal-features or a terminfo `Tc`/`RGB`), else its
/// name read as `ansi::depth` reads `TERM`.
fn client_depth(name: &str, features: &str) -> Option<Depth> {
    if name.is_empty() {
        return None;
    }
    Some(if features.split(',').any(|f| f == "RGB") {
        Depth::True
    } else if name.contains("256color") || name.contains("direct") {
        Depth::Ansi256
    } else {
        Depth::Ansi16
    })
}

/// Depths in the order of how much they show.
pub fn rank(depth: Depth) -> u8 {
    match depth {
        Depth::None => 0,
        Depth::Ansi16 => 1,
        Depth::Ansi256 => 2,
        Depth::True => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SeaDrive console: a viewer attached as `xterm` bounds the session
    /// to sixteen colours, whatever the other viewer could show.
    #[test]
    fn the_least_capable_client_of_this_session_bounds_the_colours() {
        let answer = "console-1\n\
                      console-1|xterm|bpaste,ccolour,clipboard,cstyle,focus,title\n\
                      console-1|xterm-256color|bpaste,clipboard\n\
                      other|linux|\n";
        assert_eq!(ceiling_of(answer), Some(Depth::Ansi16));
        let wide = "s\ns|xterm-256color|clipboard\ns|xterm-kitty|RGB,clipboard\n";
        assert_eq!(ceiling_of(wide), Some(Depth::Ansi256));
        assert_eq!(
            ceiling_of("s\ns|xterm-kitty|clipboard,RGB,title\n"),
            Some(Depth::True)
        );
        assert_eq!(ceiling_of("s\ns|foot-direct|\n"), Some(Depth::Ansi256));
    }

    /// Nothing attached to this session, or nothing said: no ceiling, and
    /// Rune's own `TERM` decides as before.
    #[test]
    fn no_client_of_this_session_sets_no_ceiling() {
        assert_eq!(ceiling_of("s\nother|xterm|\n"), None);
        assert_eq!(ceiling_of("s\n"), None);
        assert_eq!(ceiling_of(""), None);
        assert_eq!(
            ceiling_of("s\ns||\n"),
            None,
            "a client with no name says nothing"
        );
    }

    #[test]
    fn depths_rank_by_how_much_they_show() {
        assert!(rank(Depth::None) < rank(Depth::Ansi16));
        assert!(rank(Depth::Ansi16) < rank(Depth::Ansi256));
        assert!(rank(Depth::Ansi256) < rank(Depth::True));
    }
}
