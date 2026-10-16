//! Text written for a person's screen, made plain for the model to read.
//!
//! A search result's title and snippet are meant as text, but vendors take
//! them from web pages and some keep a page's markup: Brave marks the words
//! that matched with `<strong>` (and 9router's Brave route passes that on),
//! and an escaped `&amp;` or `&#39;` turns up wherever a page was quoted. The
//! model is handed the words — the same words whichever vendor answered —
//! never the tags around them.
//!
//! Only tags that format words inline are taken out, so a snippet about code
//! keeps its `Vec<String>`; entities are decoded after the tags are gone, so
//! an escaped `&lt;b&gt;` stays the text `<b>` it meant.

/// Tags that only format the words between them.
const INLINE: &[&str] = &[
    "a", "abbr", "b", "bdi", "bdo", "big", "br", "cite", "code", "del", "dfn", "em", "font", "i",
    "ins", "kbd", "mark", "q", "s", "samp", "small", "span", "strike", "strong", "sub", "sup",
    "time", "tt", "u", "var", "wbr",
];

/// `text` as one plain line: inline tags taken out (a `<br>` becomes a
/// space), entities decoded, every run of white space one space.
pub(crate) fn plain_line(text: &str) -> String {
    let decoded = decode_entities(&without_inline_tags(text));
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` with the inline formatting tags removed and their words kept.
fn without_inline_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        match inline_tag(rest) {
            Some((len, name)) => {
                if name == "br" {
                    out.push(' ');
                }
                rest = &rest[len..];
            }
            None => {
                out.push('<');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The length of the inline tag `text` starts with, and its name — `None`
/// when `text` starts with anything else, a `<` that is just a `<` included.
fn inline_tag(text: &str) -> Option<(usize, String)> {
    let body = text.strip_prefix('<')?;
    let body = body.strip_prefix('/').unwrap_or(body);
    let name_len = body
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(body.len());
    if name_len == 0 {
        return None;
    }
    let name = body[..name_len].to_ascii_lowercase();
    if !INLINE.contains(&name.as_str()) {
        return None;
    }
    let after = &body[name_len..];
    // The name ends the tag, or white space opens its attributes; `<bold>`
    // is not `<b>`.
    if !(after.starts_with('>')
        || after.starts_with("/>")
        || after.starts_with(char::is_whitespace))
    {
        return None;
    }
    let close = after.find(['>', '<'])?;
    if after.as_bytes()[close] != b'>' {
        return None;
    }
    Some((text.len() - after.len() + close + 1, name))
}

/// `input` with its HTML entities decoded: the named ones a page commonly
/// uses and every numeric one; anything else is left as written.
pub(crate) fn decode_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '&' {
            let mut entity = String::new();
            let mut found_semi = false;
            while let Some(&next) = chars.peek() {
                if next == ';' {
                    chars.next();
                    found_semi = true;
                    break;
                } else if next.is_alphanumeric() || next == '#' {
                    entity.push(chars.next().unwrap());
                    if entity.len() > 10 {
                        break;
                    }
                } else {
                    break;
                }
            }
            if found_semi && decode_entity_into(&entity, &mut out) {
                // Decoded into out
            } else {
                out.push('&');
                out.push_str(&entity);
                if found_semi {
                    out.push(';');
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn decode_entity_into(entity: &str, out: &mut String) -> bool {
    let s = match entity {
        "quot" => "\"",
        "amp" => "&",
        "apos" => "'",
        "lt" => "<",
        "gt" => ">",
        "nbsp" => " ",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "mdash" => "—",
        "ndash" => "–",
        "hellip" => "…",
        "lsquo" | "rsquo" => "'",
        "ldquo" | "rdquo" => "\"",
        "bull" => "•",
        "deg" => "°",
        "plusmn" => "±",
        "times" => "×",
        "divide" => "÷",
        "euro" => "€",
        "pound" => "£",
        "yen" => "¥",
        _ => {
            if let Some(hex) = entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
            {
                if let Ok(cp) = u32::from_str_radix(hex, 16) {
                    if let Some(c) = char::from_u32(cp) {
                        out.push(c);
                        return true;
                    }
                }
            } else if let Some(dec) = entity.strip_prefix('#') {
                if let Ok(cp) = dec.parse::<u32>() {
                    if let Some(c) = char::from_u32(cp) {
                        out.push(c);
                        return true;
                    }
                }
            }
            return false;
        }
    };
    out.push_str(s);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marks_a_vendor_puts_on_matched_words_go_and_the_words_stay() {
        assert_eq!(
            plain_line("The <strong>Rust</strong> Programming <em>Language</em> &amp; its book"),
            "The Rust Programming Language & its book"
        );
        assert_eq!(
            plain_line("<a href=\"https://x.dev\" class='l'>link</a> text<br/>next<BR>line"),
            "link text next line"
        );
        assert_eq!(
            plain_line("it&#39;s &quot;here&quot;&nbsp;now"),
            "it's \"here\" now"
        );
    }

    #[test]
    fn code_and_comparisons_in_a_snippet_are_text_not_tags() {
        assert_eq!(
            plain_line("fn f() -> Vec<String> where a < b && c > d"),
            "fn f() -> Vec<String> where a < b && c > d"
        );
        assert_eq!(
            plain_line("<code>Option&lt;T&gt;</code> and <bold>not a tag</bold>"),
            "Option<T> and <bold>not a tag</bold>",
            "an escaped tag stays the text it meant; an unknown name is no tag"
        );
        assert_eq!(plain_line("a <b unclosed"), "a <b unclosed");
        assert_eq!(plain_line("x <b <i>y</i>"), "x <b y");
    }

    #[test]
    fn white_space_of_any_kind_becomes_one_space() {
        assert_eq!(plain_line("  one\n\ttwo  \r\n three "), "one two three");
        assert_eq!(plain_line(""), "");
    }

    #[test]
    fn entities_decode_by_name_and_number_and_unknown_ones_stay() {
        assert_eq!(
            decode_entities("&lt;&#x41;&#66;&gt; &bogus; & x"),
            "<AB> &bogus; & x"
        );
    }
}
