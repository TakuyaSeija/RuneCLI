//! Cookies kept for one model-chosen fetch, across its redirects — never
//! longer.
//!
//! Some sites answer an anonymous reader with a sign-in check: a redirect that
//! sets a cookie, a round trip through the sign-in host, and a callback that
//! sends the reader back only once the cookie comes with it (ai.google.dev,
//! measured 2026-10-21: a loop without cookies, four hops then the page with
//! them). A jar lives in memory for one `web_fetch` call and is dropped with
//! it, so nothing a site sets reaches another call, another session or disk.
//!
//! Every cookie is host-only, whatever its `Domain` says. A browser honours
//! `Domain` against the public-suffix list, which this program does not carry;
//! without it a page could plant a cookie for every site under a shared
//! suffix and have it sent on along the same redirect chain. Host-only is
//! stricter than a browser, never wider: a cookie returns only to the host
//! that set it. `SameSite` is not read — the reader is the top-level
//! navigation, the case where a browser sends `Lax` and `None` cookies too.
//!
//! Written here rather than pulled in: what a redirect chain needs is a
//! handful of RFC 6265 rules (paths, `Secure`, expiry), and the rest of a
//! cookie store — persistence, suffix lists, scripts — is what is refused.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Cookies one jar keeps; what a site sets beyond them is ignored.
const MAX_COOKIES: usize = 64;
/// Bytes of one cookie's name and value together (RFC 6265 §6.1's floor).
const MAX_COOKIE_BYTES: usize = 4096;

#[derive(Debug, Default)]
pub struct Jar {
    cookies: Vec<Cookie>,
}

#[derive(Debug)]
struct Cookie {
    name: String,
    value: String,
    host: String,
    path: String,
    secure: bool,
    expires: Option<SystemTime>,
}

/// What a jar needs of an address: its scheme, host and path.
struct Place {
    secure: bool,
    host: String,
    path: String,
}

impl Place {
    fn of(url: &str) -> Option<Place> {
        let uri: ureq::http::Uri = url.parse().ok()?;
        let secure = match uri.scheme_str()? {
            "https" => true,
            "http" => false,
            _ => return None,
        };
        Some(Place {
            secure,
            host: uri.host()?.to_ascii_lowercase(),
            path: uri.path().to_string(),
        })
    }
}

impl Jar {
    pub fn new() -> Jar {
        Jar::default()
    }

    /// Keep what the response from `url` set, each `Set-Cookie` header as
    /// sent. A cookie that is malformed, too large, `Secure` over plain http,
    /// or one past [`MAX_COOKIES`] is ignored; one already expired removes
    /// its namesake.
    pub fn store(&mut self, url: &str, set_cookies: &[String]) {
        let Some(place) = Place::of(url) else {
            return;
        };
        let now = SystemTime::now();
        for line in set_cookies {
            let Some(cookie) = parse(line, &place, now) else {
                continue;
            };
            self.cookies.retain(|c| {
                !(c.name == cookie.name && c.host == cookie.host && c.path == cookie.path)
            });
            if cookie.expires.is_some_and(|at| at <= now) {
                continue;
            }
            if self.cookies.len() < MAX_COOKIES {
                self.cookies.push(cookie);
            }
        }
    }

    /// The `Cookie` header for a request to `url`, if any cookie belongs to
    /// it: longer paths first (RFC 6265 §5.4).
    pub fn header(&self, url: &str) -> Option<String> {
        let place = Place::of(url)?;
        let now = SystemTime::now();
        let mut matching: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|c| {
                c.host == place.host
                    && (place.secure || !c.secure)
                    && c.expires.is_none_or(|at| at > now)
                    && path_matches(&place.path, &c.path)
            })
            .collect();
        if matching.is_empty() {
            return None;
        }
        matching.sort_by_key(|c| std::cmp::Reverse(c.path.len()));
        Some(
            matching
                .iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

/// One `Set-Cookie` line, read for the request it answered (RFC 6265 §5.2).
fn parse(line: &str, place: &Place, now: SystemTime) -> Option<Cookie> {
    let mut parts = line.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let (name, value) = (name.trim(), value.trim());
    // Only what can travel back as a header unchanged: visible ASCII, no
    // separator that would end it early.
    let sendable = |s: &str| s.bytes().all(|b| (0x21..0x7f).contains(&b) && b != b';');
    if name.is_empty()
        || !sendable(name)
        || name.contains('=')
        || !(value.is_empty() || sendable(value))
        || name.len() + value.len() > MAX_COOKIE_BYTES
    {
        return None;
    }
    let mut path = None;
    let mut secure = false;
    let mut max_age = None;
    let mut expires = None;
    for attribute in parts {
        let (key, val) = match attribute.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (attribute.trim(), ""),
        };
        match key.to_ascii_lowercase().as_str() {
            "path" if val.starts_with('/') => path = Some(val.to_string()),
            "secure" => secure = true,
            "max-age" => {
                if let Ok(seconds) = val.parse::<i64>() {
                    max_age = Some(if seconds <= 0 {
                        UNIX_EPOCH
                    } else {
                        now + Duration::from_secs(seconds as u64)
                    });
                }
            }
            "expires" => expires = cookie_date(val),
            _ => {}
        }
    }
    // A cookie that claims to be secure may only be set by a secure origin
    // (RFC 6265bis): a plain-http hop cannot overwrite one an https hop set.
    if secure && !place.secure {
        return None;
    }
    Some(Cookie {
        name: name.to_string(),
        value: value.to_string(),
        host: place.host.clone(),
        path: path.unwrap_or_else(|| default_path(&place.path)),
        secure,
        expires: max_age.or(expires),
    })
}

/// The directory of the request's path (RFC 6265 §5.1.4).
fn default_path(request: &str) -> String {
    match request.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(at) => request[..at].to_string(),
    }
}

/// Whether a request's path falls under a cookie's (RFC 6265 §5.1.4).
fn path_matches(request: &str, cookie: &str) -> bool {
    request == cookie
        || (request.starts_with(cookie)
            && (cookie.ends_with('/') || request[cookie.len()..].starts_with('/')))
}

/// An `Expires` date, read as RFC 6265 §5.1.1 reads one: tokens split on
/// delimiters, then a time, a day, a month and a year found among them —
/// the spellings servers actually send (`Thu, 01 Jan 1970 00:00:00 GMT`,
/// `Thu, 01-Jan-70 …`) all fall out of the same reading.
fn cookie_date(text: &str) -> Option<SystemTime> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let (mut time, mut day, mut month, mut year) = (None, None, None, None);
    let delimiter = |c: char| !(c.is_ascii_alphanumeric() || c == ':');
    for token in text.split(delimiter).filter(|t| !t.is_empty()) {
        let digits = token.chars().take_while(char::is_ascii_digit).count();
        if time.is_none() && token.contains(':') {
            let mut hms = token.split(':').map(|p| p.parse::<u64>().ok());
            if let (Some(Some(h)), Some(Some(m)), Some(Some(s))) =
                (hms.next(), hms.next(), hms.next())
            {
                time = Some((h, m, s));
                continue;
            }
        }
        if day.is_none() && (1..=2).contains(&digits) {
            day = token[..digits].parse::<u64>().ok();
            continue;
        }
        if month.is_none() {
            let lower = token.to_ascii_lowercase();
            if let Some(at) = MONTHS.iter().position(|m| lower.starts_with(m)) {
                month = Some(at as u64 + 1);
                continue;
            }
        }
        if year.is_none() && (2..=4).contains(&digits) {
            year = token[..digits].parse::<u64>().ok();
        }
    }
    let (h, m, s) = time?;
    let (day, month, mut year) = (day?, month?, year?);
    year += match year {
        70..=99 => 1900,
        0..=69 => 2000,
        _ => 0,
    };
    if !(1..=31).contains(&day) || year < 1601 || h > 23 || m > 59 || s > 59 {
        return None;
    }
    let days = days_from_civil(year as i64, month as i64, day as i64);
    let seconds = days * 86_400 + (h * 3600 + m * 60 + s) as i64;
    Some(if seconds <= 0 {
        UNIX_EPOCH
    } else {
        UNIX_EPOCH + Duration::from_secs(seconds as u64)
    })
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// A cookie returns only to the host that set it, whatever its `Domain`
    /// says: a redirect chain cannot plant one on another site.
    #[test]
    fn a_cookie_returns_only_to_the_host_that_set_it() {
        let mut jar = Jar::new();
        jar.store(
            "https://ai.example.dev/docs/pricing",
            &lines(&[
                "csrf=abc; Path=/; Secure; HttpOnly; SameSite=None",
                "wide=1; Domain=example.dev; Path=/",
            ]),
        );
        assert_eq!(
            jar.header("https://ai.example.dev/oauth2callback?state=x")
                .as_deref(),
            Some("csrf=abc; wide=1")
        );
        for elsewhere in [
            "https://accounts.example.dev/auth",
            "https://example.dev/",
            "https://other.test/",
        ] {
            assert_eq!(jar.header(elsewhere), None, "{elsewhere}");
        }
    }

    #[test]
    fn a_secure_cookie_is_neither_set_nor_sent_over_plain_http() {
        let mut jar = Jar::new();
        jar.store("http://site.test/", &lines(&["s=1; Secure", "p=2"]));
        assert_eq!(jar.header("http://site.test/").as_deref(), Some("p=2"));
        jar.store("https://site.test/", &lines(&["s=1; Secure"]));
        assert_eq!(jar.header("http://site.test/").as_deref(), Some("p=2"));
        assert_eq!(
            jar.header("https://site.test/").as_deref(),
            Some("p=2; s=1"),
            "equal paths in the order they were set (RFC 6265 §5.4)"
        );
    }

    #[test]
    fn paths_follow_rfc_6265_and_longer_paths_go_first() {
        let mut jar = Jar::new();
        jar.store(
            "https://site.test/a/b/page",
            &lines(&["deflt=1", "root=2; Path=/", "deep=3; Path=/a/b/c"]),
        );
        assert_eq!(
            jar.header("https://site.test/a/b/x").as_deref(),
            Some("deflt=1; root=2")
        );
        assert_eq!(
            jar.header("https://site.test/a/b/c/d").as_deref(),
            Some("deep=3; deflt=1; root=2")
        );
        assert_eq!(
            jar.header("https://site.test/a/bc").as_deref(),
            Some("root=2")
        );
        assert!(path_matches("/a/b", "/a/b"));
        assert!(!path_matches("/a/bc", "/a/b"));
        assert_eq!(default_path("/"), "/");
        assert_eq!(default_path("/top"), "/");
        assert_eq!(default_path(""), "/");
    }

    /// An expired cookie removes its namesake, by `Max-Age` (which wins) or
    /// by `Expires`; a later one replaces it.
    #[test]
    fn an_expired_cookie_removes_its_namesake_and_a_new_one_replaces_it() {
        let mut jar = Jar::new();
        jar.store(
            "https://site.test/",
            &lines(&["session=old; Path=/", "keep=1; Path=/"]),
        );
        jar.store(
            "https://site.test/",
            &lines(&["session=; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=0; Path=/"]),
        );
        assert_eq!(jar.header("https://site.test/").as_deref(), Some("keep=1"));
        jar.store(
            "https://site.test/",
            &lines(&["keep=2; Path=/; Max-Age=60; Expires=Thu, 01-Jan-70 00:00:00 GMT"]),
        );
        assert_eq!(jar.header("https://site.test/").as_deref(), Some("keep=2"));
        jar.store(
            "https://site.test/",
            &lines(&["keep=gone; Path=/; Expires=Wed, 21 Oct 2015 07:28:00 GMT"]),
        );
        assert_eq!(jar.header("https://site.test/"), None);
    }

    #[test]
    fn what_could_not_travel_back_as_a_header_is_ignored() {
        let mut jar = Jar::new();
        jar.store(
            "https://site.test/",
            &lines(&[
                "",
                "=v",
                "noequals",
                "bad name=1",
                "ctl=a\u{1}b",
                "ok=1",
                &format!("big={}", "x".repeat(MAX_COOKIE_BYTES)),
            ]),
        );
        assert_eq!(jar.header("https://site.test/").as_deref(), Some("ok=1"));
        let many: Vec<String> = (0..MAX_COOKIES + 10).map(|i| format!("c{i}=1")).collect();
        let mut full = Jar::new();
        full.store("https://site.test/", &many);
        assert_eq!(full.cookies.len(), MAX_COOKIES);
        assert_eq!(Jar::new().header("not a url"), None);
    }

    #[test]
    fn cookie_dates_are_read_in_the_spellings_servers_send() {
        let at = |s: &str| cookie_date(s).map(|t| t.duration_since(UNIX_EPOCH).unwrap().as_secs());
        assert_eq!(at("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(at("Wed, 21 Oct 2015 07:28:00 GMT"), Some(1_445_412_480));
        assert_eq!(at("Wednesday, 21-Oct-15 07:28:00 GMT"), Some(1_445_412_480));
        assert_eq!(at("Tue, 20 Oct 2026 22:16:56 GMT"), Some(1_792_534_616));
        assert_eq!(at("29 Feb 2024 12:00:00"), Some(1_709_208_000));
        assert_eq!(at("not a date"), None);
        assert_eq!(at("Thu, 32 Jan 1970 00:00:00 GMT"), None);
    }
}
