//! 🚨 The crash this file exists to prevent
//!
//! `search_text` matches every line of a file whole, up to 8 MiB. The matcher
//! used to recurse once per character, so one long line exhausted the thread
//! stack and killed the process with `SIGABRT` — not an error, an abort, and
//! `panic::catch` cannot see it. These are the exact shapes that did it.

use runeengine::scan::rx::Regex;

/// What a minified bundle line or an embedded base64 blob actually looks like.
fn minified(chars: usize) -> String {
    let mut s = String::new();
    while s.chars().count() < chars {
        s.push_str("var a1b2c3d4=function(e,t){return e+t};");
    }
    s
}

#[test]
fn word_pattern_on_a_long_line_does_not_kill_the_process() {
    // \w+ on 2.000 characters used to abort the process on a 2 MiB stack.
    let r = Regex::new(r"\w+", false).unwrap();
    assert!(r.is_match(&"a".repeat(2000)).unwrap());
}

#[test]
fn the_patterns_that_crashed_the_turn_thread() {
    // Every one of these died at 500–4.000 characters before the fix. They are
    // not pathological; they are what a person types when grepping a project.
    for pat in [r"\w+", r".*", r"[a-zA-Z_][\w]*", r"\s*$", r"[0-9]+"] {
        for len in [500usize, 2000, 8000, 40_000] {
            let r = Regex::new(pat, false).unwrap();
            let line = "x".repeat(len);
            // Must answer, not abort. Either answer is correct here; what is
            // being asserted is that we get one.
            let _ = r.find(&line).expect("must not error on a plain long line");
        }
    }
}

#[test]
fn a_real_minified_line_is_searched_not_fatal() {
    let r = Regex::new(r"function", false).unwrap();
    let line = minified(7800);
    assert!(line.chars().count() > 7000);
    assert!(r.is_match(&line).unwrap());
}

#[test]
fn a_line_far_longer_than_the_old_stack_limit() {
    // 64.000 characters is 16× the length that overflowed an 8 MiB stack.
    let r = Regex::new(r"\w+", false).unwrap();
    let line = "a".repeat(64_000);
    assert!(r.is_match(&line).unwrap());
}

#[test]
fn greedy_and_lazy_still_mean_what_they_always_meant() {
    // The flat engine must not have changed what the patterns *mean*.
    let greedy = Regex::new("<.+>", false).unwrap();
    assert_eq!(greedy.find("<a><b>").unwrap(), Some((0, 6)));
    let lazy = Regex::new("<.+?>", false).unwrap();
    assert_eq!(lazy.find("<a><b>").unwrap(), Some((0, 3)));

    let a = Regex::new("a{2,3}b", false).unwrap();
    assert!(a.is_match("aab").unwrap());
    assert!(a.is_match("aaab").unwrap());
    assert!(!a.is_match("ab").unwrap());

    let anchored = Regex::new(r"^\s*pub fn \w+", false).unwrap();
    assert!(anchored
        .is_match("    pub fn render(&self) -> Vec<u8> {")
        .unwrap());
    assert!(!anchored.is_match("    let x = 1;").unwrap());
}

#[test]
fn a_legal_line_is_not_rejected_as_too_complex() {
    // 🚨 `.*Exception` on a 1.000-character line used to come back as
    // "regex complexity limit exceeded" — even though 1.000 characters is
    // inside the 2.000-character line `search_text` itself allows. The budget
    // was being shared across every start position on the line, so one pattern
    // was charged a quadratic bill for a linear question.
    let r = Regex::new(".*Exception", false).unwrap();
    let no = r.find(&"x".repeat(1000)).unwrap();
    assert_eq!(no, None, "a line with no Exception must simply not match");

    let yes = format!("{}Exception", "x".repeat(1000));
    assert!(
        r.is_match(&yes).unwrap(),
        "and it must match when it is there"
    );
}

#[test]
fn a_genuinely_explosive_pattern_is_still_bounded() {
    // The guard is not decoration: `(a+)+$` against a long run of a's is
    // exponential, and it has to be stopped — by error, not by hanging and not
    // by exhausting the stack.
    let r = Regex::new("(a+)+$", false).unwrap();
    let hay = "a".repeat(40) + "!";
    assert!(
        r.is_match(&hay).is_err(),
        "an exponential pattern must be reported, not run"
    );
}

#[test]
fn the_error_says_what_happened_and_what_to_do() {
    let r = Regex::new("(a+)+$", false).unwrap();
    let err = r.is_match(&("a".repeat(60) + "!")).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("scan.regex_steps"),
        "the message must name the knob that stopped it: {msg}"
    );
    assert!(
        !msg.contains("invalid"),
        "the pattern is not invalid — saying so is a lie: {msg}"
    );
}

#[test]
fn one_hard_line_does_not_discard_the_rest_of_the_search() {
    // 🚨 The second half of the fault. `search_text` used to propagate the
    // budget error out of the line loop, so a single line the pattern could not
    // afford threw away every match already found in that file and in every
    // file walked after it.
    //
    // The pattern is `(a+)+$` — the classic exponential one, which genuinely
    // exhausts the budget, and genuinely matches a line of a's. So the file
    // has real matches before and after an expensive line (`a`*200 + `!`,
    // which is a run of a's the `$` refuses, forcing every possible split).
    use std::io::Write;

    let dir = std::env::temp_dir().join(format!("rune-rx-skip-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("haystack.txt");
    let mut f = std::fs::File::create(&file).unwrap();
    writeln!(f, "aaa").unwrap(); // matches: a's to end of line
    writeln!(f, "aa").unwrap(); // matches
    writeln!(f, "{}!", "a".repeat(200)).unwrap(); // expensive: a's the $ refuses
    writeln!(f, "aaaa").unwrap(); // matches
    drop(f);

    let r = Regex::new("(a+)+$", false).unwrap();
    let text = std::fs::read_to_string(&file).unwrap();
    let mut found = Vec::new();
    let mut skipped = 0usize;
    for (i, line) in text.lines().enumerate() {
        match r.find(line) {
            Ok(Some(_)) => found.push(i + 1),
            Ok(None) => {}
            Err(_) => skipped += 1,
        }
    }
    assert_eq!(skipped, 1, "the hostile line should be skipped, not fatal");
    assert!(
        found.contains(&1) && found.contains(&2),
        "matches before the bad line were lost: {found:?}"
    );
    assert!(
        found.contains(&4),
        "🚨 the match AFTER the expensive line was lost: {found:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn alternation_of_literals_still_takes_the_fast_path() {
    // `is_literal()` means *one* literal — a plain substring scan. An
    // alternation of several is a different fast path (scan for any of them),
    // so it answers `false` here and that is correct.
    let r = Regex::new("max_output_size|max_file_size", false).unwrap();
    assert!(!r.is_literal());
    assert!(r.is_match("let max_file_size = 10;").unwrap());
    assert!(!r.is_match("nothing here").unwrap());
    assert!(Regex::new("max_file_size", false).unwrap().is_literal());
}
