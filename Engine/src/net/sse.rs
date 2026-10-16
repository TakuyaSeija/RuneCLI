//! Server-sent events, read one field at a time.
//!
//! Written here rather than pulled in, for the same reason the glob matcher and
//! the directory walk are: the format is a handful of rules (§9.2 of the HTML
//! spec) and the whole of what we need is `data:` lines separated by blank ones.
//! TLS is borrowed because getting it wrong is a vulnerability; this is a line
//! reader, and getting it wrong is a test failure.
//!
//! What it must get right, and what the tests hold it to:
//!
//! * a blank line ends an event; several `data:` lines in one event join with
//!   newlines, which is how a provider sends a JSON body containing newlines
//! * `:` at the start is a comment — several providers use it as a keep-alive
//!   heartbeat, and treating one as data corrupts the stream
//! * one leading space after the colon is part of the syntax, not the payload
//! * a chunk boundary can fall anywhere, including inside a UTF-8 character —
//!   so bytes are buffered and only decoded at a line boundary

use std::io::Read;

/// One `data:` payload, already joined and stripped.
pub type Data = String;

pub struct Sse<R: Read> {
    inner: R,
    buf: Vec<u8>,
    /// Bytes read but not yet consumed, as an offset into `buf`.
    at: usize,
    done: bool,
    /// `data:` lines of the event currently being assembled.
    fields: Vec<String>,
    max_bytes: usize,
    event_bytes: usize,
    pending_name: String,
    last_name: String,
}

impl<R: Read> Sse<R> {
    pub fn new(inner: R) -> Sse<R> {
        Sse {
            inner,
            buf: Vec::with_capacity(8 * 1024),
            at: 0,
            done: false,
            fields: Vec::new(),
            max_bytes: crate::settings::configuration::Configuration::default()
                .number("network.max_event_size") as usize,
            event_bytes: 0,
            pending_name: String::new(),
            last_name: String::new(),
        }
    }

    pub fn with_limit(mut self, max: usize) -> Self {
        self.max_bytes = max;
        self
    }

    pub fn event_name(&self) -> &str {
        &self.last_name
    }

    /// The next complete event's data, or `None` at end of stream.
    ///
    /// Blocks until one arrives. Errors are I/O errors — a malformed line is not
    /// an error, it is a line this format says to ignore.
    pub fn next_event(&mut self) -> std::io::Result<Option<Data>> {
        loop {
            match self.next_line()? {
                None => {
                    // End of stream. A final event without its blank line is
                    // still an event: providers do close abruptly.
                    if self.fields.is_empty() {
                        return Ok(None);
                    }
                    return Ok(Some(self.take_event()));
                }
                Some(line) => {
                    let line = line.strip_suffix('\r').unwrap_or(&line).to_string();
                    if line.is_empty() {
                        if !self.fields.is_empty() {
                            return Ok(Some(self.take_event()));
                        }
                        self.pending_name.clear();
                        continue;
                    }
                    if line.starts_with(':') {
                        continue; // comment / heartbeat
                    }
                    let (name, rest) = line.split_once(':').unwrap_or((&line, ""));
                    if name == "event" {
                        self.pending_name = rest.strip_prefix(' ').unwrap_or(rest).to_string();
                    }
                    if name == "data" {
                        self.event_bytes = self.event_bytes.saturating_add(rest.len() + 1);
                        if self.event_bytes > self.max_bytes {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "SSE event exceeds configured limit",
                            ));
                        }
                        self.fields
                            .push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                    }
                }
            }
        }
    }

    fn take_event(&mut self) -> Data {
        let joined = self.fields.join("\n");
        self.fields.clear();
        self.event_bytes = 0;
        self.last_name = std::mem::take(&mut self.pending_name);
        joined
    }

    /// One line, without its terminator. `None` at end of stream.
    fn next_line(&mut self) -> std::io::Result<Option<String>> {
        loop {
            if let Some(pos) = self.buf[self.at..].iter().position(|&b| b == b'\n') {
                let end = self.at + pos;
                let line = String::from_utf8(self.buf[self.at..end].to_vec()).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid UTF-8 in SSE data",
                    )
                })?;
                self.at = end + 1;
                return Ok(Some(line));
            }
            if self.done {
                if self.at >= self.buf.len() {
                    return Ok(None);
                }
                let line = String::from_utf8(self.buf[self.at..].to_vec()).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid UTF-8 in SSE data",
                    )
                })?;
                self.at = self.buf.len();
                return Ok(Some(line));
            }
            // Reclaim what has been consumed before reading more, so a long
            // stream does not grow the buffer without bound.
            if self.at > 0 {
                self.buf.drain(..self.at);
                self.at = 0;
            }
            let mut chunk = [0u8; 4096];
            match self.inner.read(&mut chunk) {
                Ok(0) => self.done = true,
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    if self.buf.len() > self.max_bytes {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "SSE line exceeds configured limit",
                        ));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(input: &str) -> Vec<String> {
        let mut sse = Sse::new(std::io::Cursor::new(input.as_bytes().to_vec()));
        let mut out = Vec::new();
        while let Some(e) = sse.next_event().unwrap() {
            out.push(e);
        }
        out
    }

    #[test]
    fn event_names_are_structural_and_reset_at_each_dispatch() {
        let mut sse = Sse::new(std::io::Cursor::new(b"event: error\r\ndata: {\r\ndata: \"message\":\"bad\"}\r\n\r\nevent: error\n\ndata: normal\n\n"));
        assert_eq!(
            sse.next_event().unwrap(),
            Some("{\n\"message\":\"bad\"}".into())
        );
        assert_eq!(sse.event_name(), "error");
        assert_eq!(sse.next_event().unwrap(), Some("normal".into()));
        assert_eq!(sse.event_name(), "");
    }

    #[test]
    fn reads_one_event_per_blank_line() {
        assert_eq!(events("data: a\n\ndata: b\n\n"), vec!["a", "b"]);
    }

    #[test]
    fn joins_multiple_data_lines_with_newlines() {
        assert_eq!(events("data: {\ndata: }\n\n"), vec!["{\n}"]);
    }

    #[test]
    fn ignores_comments_and_other_fields() {
        assert_eq!(
            events(": keep-alive\nevent: ping\ndata: real\n\n"),
            vec!["real"]
        );
    }

    #[test]
    fn strips_exactly_one_leading_space() {
        assert_eq!(events("data:  two spaces\n\n"), vec![" two spaces"]);
    }

    #[test]
    fn tolerates_crlf() {
        assert_eq!(events("data: a\r\n\r\n"), vec!["a"]);
    }

    #[test]
    fn yields_a_final_event_that_never_got_its_blank_line() {
        assert_eq!(events("data: last"), vec!["last"]);
    }

    #[test]
    fn survives_a_chunk_boundary_inside_a_character() {
        // A reader that hands over one byte at a time is the worst case a real
        // socket can produce, and "é" spans two of them.
        struct OneByte(Vec<u8>, usize);
        impl Read for OneByte {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                if self.1 >= self.0.len() {
                    return Ok(0);
                }
                out[0] = self.0[self.1];
                self.1 += 1;
                Ok(1)
            }
        }
        let mut sse = Sse::new(OneByte("data: café\n\n".as_bytes().to_vec(), 0));
        assert_eq!(sse.next_event().unwrap().unwrap(), "café");
    }
}
