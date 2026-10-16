//! The owner's source rule is enforced without confusing test data with shipped
//! literals. The lexical mask is only for locating cfg(test) item boundaries;
//! the inspected bytes are always the original source, comments included.
use std::path::{Path, PathBuf};

fn source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            source_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn emoji(cp: u32) -> bool {
    matches!(cp,
        0x1f000..=0x1faff | 0x2300..=0x23ff | 0x2600..=0x27bf |
        0x25fd..=0x25fe | 0x2b1b..=0x2b1c | 0x2b50 | 0x2b55 | 0x3030 | 0x303d | 0x3297 | 0x3299 | 0xfe0f
    ) && !matches!(cp,
        // Functional monochrome controls, not emoticons or decorative emoji.
        0x2315 | 0x2328 | 0x23ce | 0x23f4..=0x23f9 |
        0x2610..=0x2612 | 0x2713 | 0x2717 | 0x276e | 0x276f
    )
}

fn lexical_mask(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = bytes.to_vec();
    let mut at = 0;
    while at < bytes.len() {
        let start = at;
        if bytes[at..].starts_with(b"//") {
            at += 2;
            while at < bytes.len() && bytes[at] != b'\n' {
                at += 1;
            }
        } else if bytes[at..].starts_with(b"/*") {
            at += 2;
            let mut depth = 1;
            while at < bytes.len() && depth > 0 {
                if bytes[at..].starts_with(b"/*") {
                    depth += 1;
                    at += 2;
                } else if bytes[at..].starts_with(b"*/") {
                    depth -= 1;
                    at += 2;
                } else {
                    at += 1;
                }
            }
        } else if bytes[at] == b'r' && bytes.get(at + 1).is_some_and(|b| *b == b'#' || *b == b'"') {
            let hashes = bytes[at + 1..].iter().take_while(|&&b| b == b'#').count();
            if bytes.get(at + 1 + hashes) != Some(&b'"') {
                at += 1;
                continue;
            }
            at += hashes + 2;
            while at < bytes.len() {
                if bytes[at] == b'"'
                    && bytes
                        .get(at + 1..at + 1 + hashes)
                        .is_some_and(|tail| tail.iter().all(|&b| b == b'#'))
                {
                    at += 1 + hashes;
                    break;
                }
                at += 1;
            }
        } else if bytes[at] == b'"' {
            at += 1;
            while at < bytes.len() {
                if bytes[at] == b'\\' {
                    at = (at + 2).min(bytes.len());
                } else if bytes[at] == b'"' {
                    at += 1;
                    break;
                } else {
                    at += 1;
                }
            }
        } else if bytes[at] == b'\'' {
            // A character literal ends at its quote. A lifetime has no such
            // quote, and must leave braces and following items visible.
            let mut end = at + 1;
            if bytes.get(end) == Some(&b'\\') {
                end += 2;
                if bytes.get(at + 2) == Some(&b'u') && bytes.get(end) == Some(&b'{') {
                    while end < bytes.len() && bytes[end] != b'}' {
                        end += 1;
                    }
                    end += 1;
                } else if bytes.get(at + 2) == Some(&b'x') {
                    end += 2;
                }
            } else if let Some(c) = text.get(end..).and_then(|s| s.chars().next()) {
                end += c.len_utf8();
            }
            if bytes.get(end) == Some(&b'\'') {
                at = end + 1;
            } else {
                at += 1;
                continue;
            }
        } else {
            at += 1;
            continue;
        }
        for (index, b) in out[start..at].iter_mut().enumerate() {
            if bytes[start + index] != b'\n' {
                *b = b' ';
            }
        }
    }
    out
}

fn production(text: &str) -> String {
    let mask = lexical_mask(text);
    let code = String::from_utf8(mask).unwrap();
    let mut out = text.as_bytes().to_vec();
    let mut consumed = 0;
    for (start, _) in code.match_indices("#[cfg(test)]") {
        if start < consumed {
            continue;
        }
        let bytes = code.as_bytes();
        let mut at = start + 12;
        let mut depth = 0;
        let mut entered = false;
        while at < bytes.len() {
            match bytes[at] {
                b'{' => {
                    depth += 1;
                    entered = true;
                }
                b'}' if entered => {
                    depth -= 1;
                    if depth == 0 {
                        at += 1;
                        break;
                    }
                }
                b';' if !entered => {
                    at += 1;
                    break;
                }
                _ => {}
            }
            at += 1;
        }
        for b in &mut out[start..at] {
            if *b != b'\n' {
                *b = b' ';
            }
        }
        consumed = at;
    }
    String::from_utf8(out).unwrap()
}

#[test]
fn source_scan_excludes_only_real_test_items_not_strings_or_following_production() {
    let data=format!("pub const X: &str = \"#[cfg(test)]\";\n#[cfg(test)]\nmod tests {{ const E: &str = {:?}; }}\npub const AFTER: &str = {:?};\n", "😀", "😀");
    let kept = production(&data);
    assert!(kept.contains("AFTER"));
    assert_eq!(kept.chars().filter(|c| emoji(*c as u32)).count(), 1);
    let data=format!("#[cfg(test)]\nfn fixture<'a>() {{ let s=r###\"}} #[cfg(test)] {{\"###; let c='}}'; /* {{ }} */ let e={:?}; }}\nfn live() {{}}", "😀");
    let kept = production(&data);
    assert!(!kept.chars().any(|c| emoji(c as u32)));
    assert!(kept.contains("fn live"));
}

#[test]
fn production_source_and_comments_have_no_emoticons_or_emoji() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut files = Vec::new();
    source_files(&root.join("Cli/src"), &mut files);
    source_files(&root.join("Engine/src"), &mut files);
    assert!(files.len() > 100);
    let mut wrong = Vec::new();
    for file in files {
        // These files are included solely behind cfg(test) in their parent.
        if file
            .file_name()
            .is_some_and(|n| n == "product_tests.rs" || n == "source_style_tests.rs")
        {
            continue;
        }
        let text = production(&std::fs::read_to_string(&file).unwrap());
        for (line, text) in text.lines().enumerate() {
            for c in text.chars().filter(|c| emoji(*c as u32)) {
                wrong.push(format!(
                    "{}:{}: U+{:04X}",
                    file.strip_prefix(root).unwrap().display(),
                    line + 1,
                    c as u32
                ));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "Emoji outside tests:\n{}",
        wrong.join("\n")
    );
}
