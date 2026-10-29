//! Browser opening takes a validated web address, never a shell command or a
//! file association. A click reviews the exact address before this boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserUrl(String);

impl BrowserUrl {
    pub fn parse(text: &str) -> crate::Result<Self> {
        if text.len() > 8192
            || text
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '\\' | '"' | '<' | '>' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        {
            return Err(crate::Error::invalid(
                "Only a complete HTTP(S) URL without control characters or spaces can be opened.",
            ));
        }
        let uri: ureq::http::Uri = text
            .parse()
            .map_err(|_| crate::Error::invalid("This is not a valid HTTP(S) URL."))?;
        if !matches!(uri.scheme_str(), Some("http" | "https"))
            || uri.host().is_none_or(str::is_empty)
            || uri
                .authority()
                .is_some_and(|host| host.as_str().contains('@'))
        {
            return Err(crate::Error::invalid("Only HTTP(S) web links without embedded credentials can be opened. File paths and other schemes are copy-only."));
        }
        Ok(Self(text.into()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_destinations_cannot_be_files_commands_or_hidden_control_sequences() {
        for url in [
            "https://example.com/a?x=1&y=2",
            "http://localhost:8080/docs",
        ] {
            assert!(BrowserUrl::parse(url).is_ok(), "{url}");
        }
        for url in [
            "file:///bin/sh",
            "javascript:alert(1)",
            "ms-settings:test",
            "https://user:pass@example.com",
            "https://example.com/\n--bad",
            "https://example.com/\"x",
            "C:\\run.cmd",
            "--help",
            "https://",
            "https://example.com\\evil",
            "https://example.com/ x",
        ] {
            assert!(BrowserUrl::parse(url).is_err(), "{url:?}");
        }
    }
}
