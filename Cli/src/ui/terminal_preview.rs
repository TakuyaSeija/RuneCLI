//! A compact command is a labelled projection, never a rewritten shell script.
//! Each displayed newline has an explicit mark and clipboard source of its own.
//!
//! A command cut at the box's edge ends in `…` and nothing more: the input's
//! own mark is that ellipsis. A count of hidden source rows belongs to a
//! preview of tool *output* (`viewport::cut_lines`), never to the command line,
//! which must stay one visual row (owner, 2026-10-31).
use super::{ansi, editor, highlight, selection::CopyPiece};

pub(super) struct Preview {
    pub text: String,
    pub copy: Vec<CopyPiece>,
}

struct Atom {
    text: String,
    source: String,
    width: usize,
}

pub(super) fn command(language: &str, commands: &[String], width: usize) -> Preview {
    let width = width.max(1);
    let projected: Vec<_> = commands
        .iter()
        .map(|source| editor::Projection::plain(source))
        .collect();
    let mut atoms = Vec::new();
    let mut used = 0;
    'commands: for (command, projection) in projected.iter().enumerate() {
        if command > 0 {
            for (text, source) in [(" ", ""), ("↵", "\n"), (" ", "")] {
                push(&mut atoms, text.into(), source.into(), &mut used);
            }
            if used > width {
                break;
            }
        }
        let mut start = 0;
        for line in projection.text.split_inclusive('\n') {
            let body = line.strip_suffix('\n').unwrap_or(line);
            let pieces = highlight::pieces(language, body, ansi::TEXT);
            let mut glyphs = ansi::editor_glyphs(body).peekable();
            while let Some((at, glyph, _)) = glyphs.next() {
                let mut end = at + glyph.len();
                let source_at = projection.source_at(start + at);
                let tab = glyph == " "
                    && commands[command][source_at..].starts_with('\t')
                    && body[at..].starts_with("    ");
                let source = if tab {
                    end = at + 4;
                    while glyphs.peek().is_some_and(|(next, _, _)| *next < end) {
                        glyphs.next();
                    }
                    "\t"
                } else {
                    glyph
                };
                push(
                    &mut atoms,
                    highlight::paint(body, &pieces, at..end),
                    source.to_owned(),
                    &mut used,
                );
                if used > width {
                    break 'commands;
                }
            }
            start += body.len();
            if line.ends_with('\n') {
                for (text, source, end) in [
                    (" ", "", start),
                    ("↵", "\n", start + 1),
                    (" ", "", start + 1),
                ] {
                    if text == " " && end == start + 1 && end == projection.text.len() {
                        continue;
                    }
                    push(
                        &mut atoms,
                        format!("{}{text}{}", ansi::MUTED, ansi::RESET),
                        source.into(),
                        &mut used,
                    );
                }
                start += 1;
                if used > width {
                    break 'commands;
                }
            }
        }
    }
    let cut = used > width;
    if cut {
        while used > width - 1 {
            let atom = atoms.pop().expect("overflow has a displayed atom");
            used -= atom.width;
        }
    }
    let mut text = String::new();
    let mut copy = Vec::new();
    let mut column = 0;
    for atom in &atoms {
        text.push_str(&atom.text);
        copy.push(CopyPiece {
            columns: column..column + atom.width,
            text: atom.source.clone(),
        });
        column += atom.width;
    }
    if cut {
        text.push_str(&format!("{}…{}", ansi::MUTED, ansi::RESET));
    }
    Preview { text, copy }
}

fn push(atoms: &mut Vec<Atom>, text: String, source: String, used: &mut usize) {
    let width = ansi::width(&text);
    *used += width;
    atoms.push(Atom {
        text,
        source,
        width,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_newlines_fill_one_row_without_inventing_shell_separators() {
        for source in [
            "mkdir a\necho abc",
            "echo one\n\n  echo two",
            "echo one\n",
            "",
        ] {
            let preview = command("sh", &[source.into()], 72);
            assert_eq!(
                ansi::plain(&preview.text),
                source.replace('\n', " ↵ ").trim_end_matches(' ')
            );
            assert_eq!(
                preview
                    .copy
                    .iter()
                    .map(|piece| piece.text.as_str())
                    .collect::<String>(),
                source
            );
            assert!(!preview.text.contains('\n'));
        }
    }

    #[test]
    fn newline_projection_is_shell_independent_without_changing_the_script() {
        for (language, source) in [
            ("sh", "mkdir a\necho abc"),
            ("powershell", "New-Item a\nWrite-Output abc"),
            ("cmd", "mkdir a\necho abc"),
        ] {
            let preview = command(language, &[source.into()], 72);
            assert_eq!(ansi::plain(&preview.text), source.replace('\n', " ↵ "));
            assert_eq!(
                preview
                    .copy
                    .iter()
                    .map(|piece| piece.text.as_str())
                    .collect::<String>(),
                source
            );
        }
    }

    #[test]
    fn truncation_marks_only_a_hidden_suffix_and_never_splits_a_glyph() {
        for source in [
            "echo abcdefghijk",
            "mkdir a\necho abc\necho last",
            "echo e\u{301}界👩🏽\u{200d}💻end",
        ] {
            for width in 1..=40 {
                let preview = command("sh", &[source.into()], width);
                assert!(
                    ansi::width(&preview.text) <= width,
                    "{source:?} {width}: {:?}",
                    preview.text
                );
                let full = source.replace('\n', " ↵ ");
                assert_eq!(
                    ansi::plain(&preview.text).ends_with('…'),
                    ansi::width(&full) > width
                );
                let copied = preview
                    .copy
                    .iter()
                    .map(|piece| piece.text.as_str())
                    .collect::<String>();
                assert!(
                    source.starts_with(&copied),
                    "{copied:?} is not a source prefix"
                );
                assert!(
                    copied.is_empty()
                        || ansi::editor_glyphs(source)
                            .any(|(at, glyph, _)| at + glyph.len() == copied.len()),
                    "copied prefix splits a glyph: {copied:?}"
                );
            }
        }
    }

    #[test]
    fn a_final_newline_fitting_exactly_does_not_invent_a_hidden_suffix() {
        let preview = command("sh", &["x\n".into()], 3);
        assert_eq!(ansi::plain(&preview.text), "x ↵");
        assert_eq!(
            preview
                .copy
                .iter()
                .map(|piece| piece.text.as_str())
                .collect::<String>(),
            "x\n"
        );
    }

    #[test]
    fn clipboard_source_keeps_tabs_and_literal_markers_but_not_terminal_controls() {
        let preview = command("sh", &["echo\t'↵ …'\necho \x1b[31msafe\x1b[0m".into()], 72);
        assert_eq!(ansi::plain(&preview.text), "echo    '↵ …' ↵ echo safe");
        assert_eq!(
            preview
                .copy
                .iter()
                .map(|piece| piece.text.as_str())
                .collect::<String>(),
            "echo\t'↵ …'\necho safe"
        );
    }

    /// A cut command says only that it was cut: `…` at the edge, no count.
    /// A hidden-row count is a tool *output* fold (`viewport::cut_lines`); the
    /// command line stays one visual row and never grows a second mark
    /// (owner, 2026-10-31).
    #[test]
    fn a_cut_command_marks_the_ellipsis_only_and_never_counts_rows() {
        for source in ["a\n\nb", "echo a\necho b\necho c"] {
            let preview = command("sh", &[source.into()], 5);
            let plain = ansi::plain(&preview.text);
            assert!(plain.ends_with('…'), "{plain:?}");
            assert!(!plain.contains("lines cut"), "{plain:?}");
            assert!(!plain.contains("lines"), "{plain:?}");
            assert!(!plain.contains('\n'), "{plain:?}");
        }
        let whole = command("sh", &["echo a\necho b\necho c".into()], 72);
        assert!(!ansi::plain(&whole.text).ends_with('…'));
    }
}
