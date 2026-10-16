//! Line diff — what a turn actually changed, hunk by hunk.
//!
//! This is engine work, not frontend work. A checkpoint holds content-addressed
//! blobs; handing those to a GUI and telling it to work out the difference would
//! push the same comparison into every frontend that is ever written, and the
//! per-hunk review the GUI is meant to offer would be built on it.
//!
//! # The algorithm, and why it is bounded
//!
//! Common prefix and suffix are trimmed first — which is nearly all of an
//! ordinary edit — and the remaining middle goes through a Myers O((N+M)·D)
//! diff: cheap while few lines differ, because both its time and its memory
//! grow with D, the number of differing lines. A middle Myers would need more
//! than [`MAX_CELLS`] of work for (D × (N+M)) is cut instead at the lines that
//! occur exactly once on each side and in the same order (patience anchors),
//! and each piece between two anchors is diffed the same way. A piece with no
//! anchor that Myers cannot afford either is all removed, then all added.
//!
//! So every pair of texts gets an edit script that rebuilds the new text from
//! the old — minimal while Myers can afford it, close to it after anchoring —
//! and the counts a frontend shows are always those of a real diff. The whole
//! comparison is bounded by [`TOTAL_WORK`] budgets: a pathological pair (every
//! line reordered) costs that much and no more.

use std::ops::Range;

use serde::Serialize;

/// The Myers work budget for one piece: Myers gives the piece up once
/// D × (N+M) passes it, where D is the number of differing lines. The name is
/// a leftover of the O(n·m) table this algorithm replaced; the config key
/// `checkpoint.diff.max_cells` keeps it so existing `rune.toml` files hold.
pub const MAX_CELLS: usize = 1_000_000;

/// How many [`MAX_CELLS`] budgets one whole comparison may spend, Myers and
/// anchoring together, before what is left is written as removed-then-added.
pub const TOTAL_WORK: usize = 16;

/// Unchanged lines kept either side of a change, as in `diff -u`.
pub const CONTEXT: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    /// Present in both, carried for context.
    Same,
    /// In the checkpoint, gone now.
    Del,
    /// Not in the checkpoint, present now.
    Add,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Line {
    pub op: Op,
    pub text: String,
}

/// One run of changes plus its context, in `@@ -a,b +c,d @@` terms. Line numbers
/// are 1-based, and a count of zero means "at this point", which is how an
/// insertion into an empty file is expressed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<Line>,
}

/// Compare two texts, hunk by hunk. Never refuses: see the module notes.
pub fn hunks(before: &str, after: &str, max_cells: usize, context: usize) -> Vec<Hunk> {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    group(&script(&a, &b, max_cells.max(1)), context)
}

/// What is left to write, in order: a stack of these is the recursion of the
/// anchored diff, kept on the heap so a deep split cannot run out of stack.
enum Task {
    /// Two ranges still to compare, common ends not yet trimmed.
    Gap(Range<usize>, Range<usize>),
    /// Two ranges whose ends differ.
    Middle(Range<usize>, Range<usize>),
    /// A line present on both sides, by its index in the old text.
    Same(usize),
}

/// The edit script that turns `a` into `b`.
fn script<'a>(a: &[&'a str], b: &[&'a str], max_cells: usize) -> Vec<(Op, &'a str)> {
    let mut out: Vec<(Op, &'a str)> = Vec::with_capacity(a.len().max(b.len()));
    let mut left = max_cells.saturating_mul(TOTAL_WORK);
    let mut tasks = vec![Task::Gap(0..a.len(), 0..b.len())];
    while let Some(task) = tasks.pop() {
        match task {
            Task::Same(i) => out.push((Op::Same, a[i])),
            Task::Gap(ra, rb) => {
                let mut head = 0;
                while head < ra.len() && head < rb.len() && a[ra.start + head] == b[rb.start + head]
                {
                    out.push((Op::Same, a[ra.start + head]));
                    head += 1;
                }
                let mut tail = 0;
                while tail < ra.len() - head
                    && tail < rb.len() - head
                    && a[ra.end - 1 - tail] == b[rb.end - 1 - tail]
                {
                    tail += 1;
                }
                // Popped last-in first-out: the tail goes on before the middle.
                tasks.extend((ra.end - tail..ra.end).rev().map(Task::Same));
                tasks.push(Task::Middle(
                    ra.start + head..ra.end - tail,
                    rb.start + head..rb.end - tail,
                ));
            }
            Task::Middle(ra, rb) => {
                let (pa, pb) = (&a[ra.clone()], &b[rb.clone()]);
                if pa.is_empty() || pb.is_empty() {
                    replace(&mut out, pa, pb);
                    continue;
                }
                let size = pa.len() + pb.len();
                if left < size {
                    replace(&mut out, pa, pb);
                    continue;
                }
                left -= size;
                let (ops, spent) = myers_diff(pa, pb, max_cells.min(left));
                left = left.saturating_sub(spent);
                if let Some(ops) = ops {
                    out.extend(ops);
                    continue;
                }
                let anchors = anchors(pa, pb);
                if anchors.is_empty() {
                    replace(&mut out, pa, pb);
                    continue;
                }
                // Pushed back to front: the gap after the last anchor first.
                let (mut end_a, mut end_b) = (ra.end, rb.end);
                for &(i, j) in anchors.iter().rev() {
                    let (i, j) = (ra.start + i, rb.start + j);
                    tasks.push(Task::Gap(i + 1..end_a, j + 1..end_b));
                    tasks.push(Task::Same(i));
                    end_a = i;
                    end_b = j;
                }
                tasks.push(Task::Gap(ra.start..end_a, rb.start..end_b));
            }
        }
    }
    out
}

/// `a` removed, then `b` added: the edit script with nothing kept.
fn replace<'a>(out: &mut Vec<(Op, &'a str)>, a: &[&'a str], b: &[&'a str]) {
    out.extend(a.iter().map(|l| (Op::Del, *l)));
    out.extend(b.iter().map(|l| (Op::Add, *l)));
}

/// The lines that occur exactly once in `a` and once in `b`, as index pairs,
/// keeping the longest run that is in order on both sides — the patience
/// diff's anchors. Ascending in both indices.
fn anchors(a: &[&str], b: &[&str]) -> Vec<(usize, usize)> {
    use std::collections::HashMap;
    // Per line: (times in a, index in a, times in b, index in b).
    let mut seen: HashMap<&str, (u32, usize, u32, usize)> = HashMap::new();
    for (i, line) in a.iter().enumerate() {
        let entry = seen.entry(line).or_insert((0, i, 0, 0));
        entry.0 += 1;
    }
    for (j, line) in b.iter().enumerate() {
        if let Some(entry) = seen.get_mut(line) {
            entry.2 += 1;
            entry.3 = j;
        }
    }
    let mut pairs: Vec<(usize, usize)> = seen
        .values()
        .filter(|(in_a, _, in_b, _)| *in_a == 1 && *in_b == 1)
        .map(|(_, i, _, j)| (*i, *j))
        .collect();
    pairs.sort_unstable();

    // Longest increasing run of `j`, by patience sorting: `tops[k]` is the
    // pair ending the best run of length k + 1 found so far.
    let mut tops: Vec<usize> = Vec::new();
    let mut before: Vec<Option<usize>> = vec![None; pairs.len()];
    for (n, &(_, j)) in pairs.iter().enumerate() {
        let k = tops.partition_point(|&t| pairs[t].1 < j);
        before[n] = k.checked_sub(1).map(|k| tops[k]);
        if k == tops.len() {
            tops.push(n);
        } else {
            tops[k] = n;
        }
    }
    let mut run = Vec::with_capacity(tops.len());
    let mut at = tops.last().copied();
    while let Some(n) = at {
        run.push(pairs[n]);
        at = before[n];
    }
    run.reverse();
    run
}

/// Myers O(ND) diff of the part that actually differs, and the work it spent.
/// Its trace keeps one row of the (N+M)-wide frontier per D, so time and
/// memory are both D × (N+M): it gives up (`None`) before that passes
/// `max_work`.
fn myers_diff<'a>(
    a: &[&'a str],
    b: &[&'a str],
    max_work: usize,
) -> (Option<Vec<(Op, &'a str)>>, usize) {
    let n = a.len();
    let m = b.len();
    if n == 0 {
        return (Some(b.iter().map(|l| (Op::Add, *l)).collect()), 0);
    }
    if m == 0 {
        return (Some(a.iter().map(|l| (Op::Del, *l)).collect()), 0);
    }

    let total_len = n + m;
    let max_d = total_len;
    let offset = max_d as isize;
    let mut v = vec![0usize; 2 * max_d + 1];
    let mut trace: Vec<Vec<usize>> = Vec::new();

    let mut found_d = None;
    for d in 0..=max_d {
        if (d + 1).saturating_mul(total_len) > max_work {
            return (None, d.saturating_mul(total_len));
        }

        trace.push(v.clone());
        for k in (-(d as isize)..=(d as isize)).step_by(2) {
            let mut x = if d == 0 {
                0
            } else if k == -(d as isize)
                || (k != (d as isize)
                    && trace[d][(k - 1 + offset) as usize] < trace[d][(k + 1 + offset) as usize])
            {
                trace[d][(k + 1 + offset) as usize]
            } else {
                trace[d][(k - 1 + offset) as usize] + 1
            };
            let mut y = (x as isize - k) as usize;

            while x < n && y < m && a[x] == b[y] {
                x += 1;
                y += 1;
            }
            v[(k + offset) as usize] = x;

            if x >= n && y >= m {
                found_d = Some(d);
                break;
            }
        }
        if found_d.is_some() {
            break;
        }
    }

    let Some(d) = found_d else {
        return (None, max_d.saturating_mul(total_len));
    };
    let spent = (d + 1).saturating_mul(total_len);

    // Backtrack from (n, m)
    let mut ops = Vec::new();
    let mut x = n;
    let mut y = m;

    for step in (1..=d).rev() {
        let v_prev = &trace[step];
        let k = x as isize - y as isize;
        let (prev_k, horiz) = if k == -(step as isize)
            || (k != (step as isize)
                && v_prev[(k - 1 + offset) as usize] < v_prev[(k + 1 + offset) as usize])
        {
            (k + 1, false)
        } else {
            (k - 1, true)
        };
        let prev_x = v_prev[(prev_k + offset) as usize];
        let prev_y = (prev_x as isize - prev_k) as usize;

        while x > prev_x + usize::from(horiz) && y > prev_y + usize::from(!horiz) {
            x -= 1;
            y -= 1;
            ops.push((Op::Same, a[x]));
        }

        if horiz {
            x -= 1;
            ops.push((Op::Del, a[prev_x]));
        } else {
            y -= 1;
            ops.push((Op::Add, b[prev_y]));
        }
    }

    while x > 0 && y > 0 {
        x -= 1;
        y -= 1;
        ops.push((Op::Same, a[x]));
    }

    ops.reverse();
    (Some(ops), spent)
}

/// Cut the edit script into hunks: every change, plus [`CONTEXT`] lines around
/// it, with runs that would overlap merged into one.
fn group(ops: &[(Op, &str)], context: usize) -> Vec<Hunk> {
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, (op, _))| *op != Op::Same)
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return Vec::new();
    }

    // Ranges of indices to emit, merged where their context touches.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &i in &changed {
        let lo = i.saturating_sub(context);
        let hi = (i + context).min(ops.len() - 1);
        match ranges.last_mut() {
            Some(last) if lo <= last.1 + 1 => last.1 = last.1.max(hi),
            _ => ranges.push((lo, hi)),
        }
    }

    // Line numbers are counted from the start of the script, once, rather than
    // recomputed per hunk — an off-by-one here is invisible until a frontend
    // applies a hunk to the wrong place.
    let mut old_no = 1usize;
    let mut new_no = 1usize;
    let mut at = 0usize;
    let mut out = Vec::with_capacity(ranges.len());

    for (lo, hi) in ranges {
        while at < lo {
            match ops[at].0 {
                Op::Same => {
                    old_no += 1;
                    new_no += 1;
                }
                Op::Del => old_no += 1,
                Op::Add => new_no += 1,
            }
            at += 1;
        }

        let (old_start, new_start) = (old_no, new_no);
        let (mut old_count, mut new_count) = (0usize, 0usize);
        let mut lines = Vec::with_capacity(hi - lo + 1);
        while at <= hi {
            let (op, text) = ops[at];
            match op {
                Op::Same => {
                    old_count += 1;
                    new_count += 1;
                    old_no += 1;
                    new_no += 1;
                }
                Op::Del => {
                    old_count += 1;
                    old_no += 1;
                }
                Op::Add => {
                    new_count += 1;
                    new_no += 1;
                }
            }
            lines.push(Line {
                op,
                text: text.to_string(),
            });
            at += 1;
        }
        out.push(Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
            lines,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(hs: &[Hunk]) -> String {
        let mut s = String::new();
        for h in hs {
            s.push_str(&format!(
                "@@ -{},{} +{},{} @@\n",
                h.old_start, h.old_count, h.new_start, h.new_count
            ));
            for l in &h.lines {
                let m = match l.op {
                    Op::Same => ' ',
                    Op::Del => '-',
                    Op::Add => '+',
                };
                s.push_str(&format!("{m}{}\n", l.text));
            }
        }
        s
    }

    #[test]
    fn identical_texts_produce_no_hunks() {
        assert!(hunks("a\nb\nc\n", "a\nb\nc\n", MAX_CELLS, CONTEXT).is_empty());
        assert!(hunks("", "", MAX_CELLS, CONTEXT).is_empty());
    }

    #[test]
    fn a_one_line_change_carries_its_context_and_nothing_more() {
        let before = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let after = "1\n2\n3\n4\nFIVE\n6\n7\n8\n9\n";
        let hs = hunks(before, after, MAX_CELLS, CONTEXT);
        assert_eq!(hs.len(), 1);
        assert_eq!(
            render(&hs),
            "@@ -2,7 +2,7 @@\n 2\n 3\n 4\n-5\n+FIVE\n 6\n 7\n 8\n"
        );
    }

    #[test]
    fn two_distant_changes_become_two_hunks() {
        let before = (1..=30).map(|n| format!("line {n}\n")).collect::<String>();
        let after: String = (1..=30)
            .map(|n| match n {
                2 => "TWO\n".to_string(),
                28 => "TWENTYEIGHT\n".to_string(),
                _ => format!("line {n}\n"),
            })
            .collect();
        let hs = hunks(&before, &after, MAX_CELLS, CONTEXT);
        assert_eq!(hs.len(), 2, "{}", render(&hs));
        assert!(hs[0].lines.iter().any(|l| l.text == "TWO"));
        assert!(hs[1].lines.iter().any(|l| l.text == "TWENTYEIGHT"));
    }

    #[test]
    fn two_adjacent_changes_merge_into_one_hunk() {
        let before = "1\n2\n3\n4\n5\n6\n7\n8\n";
        let after = "1\nTWO\n3\n4\n5\nSIX\n7\n8\n";
        let hs = hunks(before, after, MAX_CELLS, CONTEXT);
        assert_eq!(
            hs.len(),
            1,
            "context overlaps, so the hunks are one: {}",
            render(&hs)
        );
    }

    #[test]
    fn a_pure_insertion_and_a_pure_deletion_are_both_expressible() {
        let added = hunks("a\nb\n", "a\nNEW\nb\n", MAX_CELLS, CONTEXT);
        assert_eq!(render(&added), "@@ -1,2 +1,3 @@\n a\n+NEW\n b\n");

        let removed = hunks("a\nGONE\nb\n", "a\nb\n", MAX_CELLS, CONTEXT);
        assert_eq!(render(&removed), "@@ -1,3 +1,2 @@\n a\n-GONE\n b\n");
    }

    #[test]
    fn writing_into_an_empty_file_starts_at_line_zero_on_the_old_side() {
        let hs = hunks("", "first\n", MAX_CELLS, CONTEXT);
        assert_eq!(render(&hs), "@@ -1,0 +1,1 @@\n+first\n");
    }

    #[test]
    fn a_replaced_line_reads_as_the_removal_before_the_addition() {
        let hs = hunks("old\n", "new\n", MAX_CELLS, CONTEXT);
        let ops: Vec<Op> = hs[0].lines.iter().map(|l| l.op).collect();
        assert_eq!(ops, vec![Op::Del, Op::Add]);
    }

    #[test]
    fn line_numbers_survive_a_change_that_shifts_everything_after_it() {
        let before = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let after = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nK\n";
        let hs = hunks(before, after, MAX_CELLS, CONTEXT);
        assert_eq!(hs[0].old_start, 8);
        assert_eq!(hs[0].new_start, 8);
        assert_eq!(hs[0].old_count, 3);
        assert_eq!(hs[0].new_count, 4);
    }

    #[test]
    fn a_large_file_with_distant_edits_is_diffed_efficiently_without_too_large() {
        let before_lines: Vec<String> = (0..7000).map(|i| format!("line {i}")).collect();
        let mut after_lines = before_lines.clone();
        after_lines[100] = "line 100 modified".into();
        after_lines[6500] = "line 6500 modified".into();
        let before = before_lines.join("\n") + "\n";
        let after = after_lines.join("\n") + "\n";

        let hs = hunks(&before, &after, MAX_CELLS, CONTEXT);
        assert_eq!(hs.len(), 2, "two distant edits become two clean hunks");
        let added_lines: usize = hs
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.op == Op::Add)
            .count();
        let removed_lines: usize = hs
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.op == Op::Del)
            .count();
        assert_eq!(added_lines, 2);
        assert_eq!(removed_lines, 2);
    }

    #[test]
    fn every_line_of_the_new_text_can_be_rebuilt_from_the_hunks() {
        // The property that matters: a frontend applying what it was given lands
        // exactly on the file that is on disk.
        let before = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
        let after = "alpha\nBETA\ngamma\nepsilon\nzeta\n";
        let hs = hunks(before, after, MAX_CELLS, CONTEXT);

        let mut rebuilt: Vec<String> = before.lines().map(str::to_string).collect();
        // Apply back to front so earlier hunks keep their line numbers.
        for h in hs.iter().rev() {
            let replacement: Vec<String> = h
                .lines
                .iter()
                .filter(|l| l.op != Op::Del)
                .map(|l| l.text.clone())
                .collect();
            let from = h.old_start - 1;
            rebuilt.splice(from..from + h.old_count, replacement);
        }
        assert_eq!(rebuilt.join("\n") + "\n", after);
    }

    /// Apply `hs` to `before`, back to front, as a frontend would.
    fn apply(before: &str, hs: &[Hunk]) -> String {
        let mut rebuilt: Vec<String> = before.lines().map(str::to_string).collect();
        for h in hs.iter().rev() {
            let replacement: Vec<String> = h
                .lines
                .iter()
                .filter(|l| l.op != Op::Del)
                .map(|l| l.text.clone())
                .collect();
            let from = h.old_start.saturating_sub(1).min(rebuilt.len());
            let from = if h.old_count == 0 {
                h.old_start.min(rebuilt.len())
            } else {
                from
            };
            rebuilt.splice(from..from + h.old_count, replacement);
        }
        rebuilt.join("\n") + "\n"
    }

    fn counts(hs: &[Hunk]) -> (usize, usize) {
        let lines = hs.iter().flat_map(|h| &h.lines);
        (
            lines.clone().filter(|l| l.op == Op::Add).count(),
            lines.filter(|l| l.op == Op::Del).count(),
        )
    }

    /// The owner's report: a big file with many scattered edits came out as
    /// "too large" and the panel showed a running tally instead of the diff.
    /// Past Myers' budget the diff is anchored, and the counts stay exact.
    #[test]
    fn many_scattered_edits_past_the_myers_budget_still_diff_exactly() {
        let before_lines: Vec<String> = (0..20_000).map(|i| format!("line {i}")).collect();
        let mut after_lines = before_lines.clone();
        for i in (0..20_000).step_by(97) {
            after_lines[i] = format!("line {i} changed");
        }
        let changed = (0..20_000).step_by(97).count();
        let before = before_lines.join("\n") + "\n";
        let after = after_lines.join("\n") + "\n";
        // D × (N+M) is far past the budget: Myers alone would refuse.
        assert!(2 * changed * 40_000 > MAX_CELLS);

        let hs = hunks(&before, &after, MAX_CELLS, CONTEXT);
        assert_eq!(counts(&hs), (changed, changed));
        assert_eq!(apply(&before, &hs), after);
    }

    /// Whatever the texts, the script rebuilds the new one — even when no line
    /// can anchor and the budget is spent, which ends removed-then-added.
    #[test]
    fn a_reordered_file_on_a_tiny_budget_still_rebuilds_the_new_text() {
        let before: String = (0..3000).map(|i| format!("{i}\n")).collect();
        let after: String = (0..3000).rev().map(|i| format!("{i}\n")).collect();
        let started = std::time::Instant::now();
        let hs = hunks(&before, &after, 1000, CONTEXT);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(apply(&before, &hs), after);
        let (added, removed) = counts(&hs);
        assert_eq!(added, removed, "a reorder adds what it removes");
        assert!(added < 3000, "anchors keep some lines: {added}");
    }

    #[test]
    fn duplicate_lines_between_anchors_are_kept_not_rewritten() {
        let before = "fn a() {\n}\n\nfn b() {\n}\n";
        let after = "fn a() {\n    x();\n}\n\nfn b() {\n}\n";
        let hs = hunks(before, after, 1, CONTEXT);
        assert_eq!(counts(&hs), (1, 0), "{}", render(&hs));
        assert_eq!(apply(before, &hs), after);
    }
}
