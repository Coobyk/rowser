//! URL parsing and resolution — RFC 3986 basics with the defaults a browser
//! needs (lowered scheme/host, well-known ports, dot-segment removal).

use std::fmt;

/// Ways a URL string can fail to parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlError {
    /// No `scheme:` prefix (or the prefix is not a valid scheme).
    MissingScheme,
    /// Authority component is empty or malformed.
    MissingHost,
    /// Port is present but not a `u16`.
    InvalidPort,
    /// Bad `%XX` escape, or decoded bytes are not UTF-8.
    InvalidPercentEncoding,
}

impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingScheme => write!(f, "missing or invalid URL scheme"),
            Self::MissingHost => write!(f, "missing URL host"),
            Self::InvalidPort => write!(f, "invalid URL port"),
            Self::InvalidPercentEncoding => write!(f, "invalid percent-encoding"),
        }
    }
}

impl std::error::Error for UrlError {}

/// A parsed absolute URL. Path/query/fragment stay percent-encoded; use
/// [`percent_decode`] (or `decoded_path()` etc.) where a decoded view is
/// needed — the wire form is what HTTP requests send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    /// Lowercased scheme, e.g. `http`.
    pub scheme: String,
    /// Lowercased host (userinfo stripped); IPv6 literals keep brackets.
    pub host: String,
    /// Explicit port, else the scheme default, else 80.
    pub port: u16,
    /// Percent-encoded path; always begins with '/'.
    pub path: String,
    /// Percent-encoded query, without the leading '?'.
    pub query: Option<String>,
    /// Fragment, without the leading '#'. Never sent to servers.
    pub fragment: Option<String>,
}

impl Url {
    /// Parse an absolute URL like `http://host:port/path?query#frag`.
    pub fn parse(input: &str) -> Result<Url, UrlError> {
        let input = input.trim();
        let (scheme, rest) = match input.find(':') {
            Some(i) if i > 0 && is_scheme(&input[..i]) => {
                (input[..i].to_ascii_lowercase(), &input[i + 1..])
            }
            _ => return Err(UrlError::MissingScheme),
        };
        let rest = rest.strip_prefix("//").unwrap_or(rest);
        let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, rest) = rest.split_at(auth_end);
        // userinfo (user[:pass]@) is discarded.
        let authority = authority.rsplit('@').next().unwrap_or_default();
        if authority.is_empty() {
            return Err(UrlError::MissingHost);
        }
        let (host, port) = parse_host_port(authority)?;
        let port = match port {
            Some(p) => p,
            None => default_port(&scheme).unwrap_or(80),
        };

        let (rest, fragment) = split_fragment(rest);
        let (path, query) = split_query(rest);
        let path = if path.is_empty() {
            "/".to_string()
        } else {
            path.to_string()
        };
        Ok(Url {
            scheme,
            host,
            port,
            path,
            query,
            fragment,
        })
    }

    /// Resolve `relative` against this URL (RFC 3986 §5.2 basics): absolute
    /// and scheme-relative references, absolute/relative paths, `?query`
    /// and `#fragment` references, dot-segment removal.
    pub fn resolve(&self, relative: &str) -> Result<Url, UrlError> {
        let rel = relative.trim();
        // A scheme-valid prefix before the first ':' means an absolute URI.
        if let Some(i) = rel.find(':') {
            if i > 0 && is_scheme(&rel[..i]) {
                return Url::parse(rel);
            }
        }
        if let Some(rest) = rel.strip_prefix("//") {
            return Url::parse(&format!("{}://{}", self.scheme, rest));
        }

        let (rel, fragment) = split_fragment(rel);
        let (rel, query) = split_query(rel);
        let mut u = self.clone();
        u.fragment = fragment;
        if rel.is_empty() {
            // Same-document reference: keep base path, maybe replace query.
            if let Some(q) = &query {
                u.query = Some(q.clone());
            }
            if query.is_none() && u.fragment.is_none() {
                u.fragment = self.fragment.clone();
            }
            return Ok(u);
        }
        u.query = query;
        if rel.starts_with('/') {
            u.path = remove_dot_segments(rel);
        } else {
            // Merge with the base path's directory (everything to last '/').
            let dir = match self.path.rfind('/') {
                Some(i) => &self.path[..i + 1],
                None => "",
            };
            u.path = remove_dot_segments(&format!("{dir}{rel}"));
        }
        Ok(u)
    }

    /// `path?query` — the target of an HTTP request line.
    pub fn request_target(&self) -> String {
        match &self.query {
            Some(q) => format!("{}?{q}", self.path),
            None => self.path.clone(),
        }
    }

    /// Percent-decoded view of the path.
    pub fn decoded_path(&self) -> Result<String, UrlError> {
        percent_decode(&self.path)
    }

    /// Percent-decoded view of the query, if present.
    pub fn decoded_query(&self) -> Result<Option<String>, UrlError> {
        self.query.as_deref().map(percent_decode).transpose()
    }

    /// Percent-decoded view of the fragment, if present.
    pub fn decoded_fragment(&self) -> Result<Option<String>, UrlError> {
        self.fragment.as_deref().map(percent_decode).transpose()
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}", self.scheme, self.host)?;
        if Some(self.port) != default_port(&self.scheme) {
            write!(f, ":{}", self.port)?;
        }
        write!(f, "{}", self.path)?;
        if let Some(q) = &self.query {
            write!(f, "?{q}")?;
        }
        if let Some(frag) = &self.fragment {
            write!(f, "#{frag}")?;
        }
        Ok(())
    }
}

/// Well-known default ports; `None` for schemes we don't know.
pub(crate) fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        "ftp" => Some(21),
        _ => None,
    }
}

/// First char ASCII alpha, rest alnum/'+'/'-'/'.' (RFC 3986 §3.1).
fn is_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// Split `host[:port]`; bracketed IPv6 literals supported.
fn parse_host_port(authority: &str) -> Result<(String, Option<u16>), UrlError> {
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']').ok_or(UrlError::MissingHost)?;
        let host = &rest[..end];
        let tail = &rest[end + 1..];
        let port = match tail.strip_prefix(':') {
            Some(p) => Some(p.parse().map_err(|_| UrlError::InvalidPort)?),
            None if tail.is_empty() => None,
            None => return Err(UrlError::InvalidPort),
        };
        return Ok((format!("[{host}]"), port));
    }
    match authority.rsplit_once(':') {
        Some((h, "")) => Ok((h.to_ascii_lowercase(), None)),
        Some((h, p)) => Ok((
            h.to_ascii_lowercase(),
            Some(p.parse().map_err(|_| UrlError::InvalidPort)?),
        )),
        None => Ok((authority.to_ascii_lowercase(), None)),
    }
}

/// Split off a `#fragment` suffix.
fn split_fragment(s: &str) -> (&str, Option<String>) {
    match s.find('#') {
        Some(i) => (&s[..i], Some(s[i + 1..].to_string())),
        None => (s, None),
    }
}

/// Split off a `?query` suffix.
fn split_query(s: &str) -> (&str, Option<String>) {
    match s.find('?') {
        Some(i) => (&s[..i], Some(s[i + 1..].to_string())),
        None => (s, None),
    }
}

/// RFC 3986 §5.2.4 dot-segment removal. `path` always begins with '/' here;
/// repeated slashes collapse (a harmless simplification for HTTP targets).
fn remove_dot_segments(path: &str) -> String {
    let trailing_slash = path.ends_with('/') || path.ends_with("/.") || path.ends_with("/..");
    let mut stack: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            s => stack.push(s),
        }
    }
    let mut out = String::from("/");
    out.push_str(&stack.join("/"));
    if (trailing_slash || stack.is_empty()) && !out.ends_with('/') {
        out.push('/');
    }
    out
}

/// Decode `%XX` escapes into a UTF-8 string. `+` is left alone (that's
/// `application/x-www-form-urlencoded`, a different layer).
pub fn percent_decode(input: &str) -> Result<String, UrlError> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err(UrlError::InvalidPercentEncoding);
                }
                let hi = hex_val(bytes[i + 1])?;
                let lo = hex_val(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| UrlError::InvalidPercentEncoding)
}

fn hex_val(b: u8) -> Result<u8, UrlError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(UrlError::InvalidPercentEncoding),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full() {
        let u = Url::parse("http://Example.COM:8080/a/b?x=1&y=2#frag").unwrap();
        assert_eq!(u.scheme, "http");
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, 8080);
        assert_eq!(u.path, "/a/b");
        assert_eq!(u.query.as_deref(), Some("x=1&y=2"));
        assert_eq!(u.fragment.as_deref(), Some("frag"));
        assert_eq!(u.request_target(), "/a/b?x=1&y=2");
    }

    #[test]
    fn parse_defaults() {
        let u = Url::parse("http://a.b").unwrap();
        assert_eq!(u.port, 80);
        assert_eq!(u.path, "/");
        assert_eq!(u.query, None);
        assert_eq!(u.fragment, None);
        assert_eq!(u.to_string(), "http://a.b/");

        assert_eq!(Url::parse("https://a.b/x").unwrap().port, 443);
        // Non-default ports round-trip through Display.
        assert_eq!(
            Url::parse("http://a.b:81/").unwrap().to_string(),
            "http://a.b:81/"
        );
        // Userinfo is stripped.
        assert_eq!(Url::parse("http://u:p@host/x").unwrap().host, "host");
        // IPv6 literal.
        assert_eq!(Url::parse("http://[::1]:8080/").unwrap().host, "[::1]");
    }

    #[test]
    fn parse_errors() {
        assert_eq!(Url::parse("noscheme"), Err(UrlError::MissingScheme));
        assert_eq!(Url::parse("1bad://x"), Err(UrlError::MissingScheme));
        assert_eq!(Url::parse("http://"), Err(UrlError::MissingHost));
        assert_eq!(Url::parse("http://h:zz/"), Err(UrlError::InvalidPort));
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("%68ello%20w%6Frld").unwrap(), "hello world");
        assert_eq!(percent_decode("plain").unwrap(), "plain");
        assert_eq!(percent_decode("a+b").unwrap(), "a+b"); // '+' untouched
        assert_eq!(percent_decode("%zz"), Err(UrlError::InvalidPercentEncoding));
        assert_eq!(
            percent_decode("cut%4"),
            Err(UrlError::InvalidPercentEncoding)
        );
        let u = Url::parse("http://h/a%20b?q=%41%42").unwrap();
        assert_eq!(u.decoded_path().unwrap(), "/a b");
        assert_eq!(u.decoded_query().unwrap().as_deref(), Some("q=AB"));
    }

    /// RFC 3986 §5.4 examples against base "http://a/b/c/d;p?q".
    fn base() -> Url {
        Url::parse("http://a/b/c/d;p?q").unwrap()
    }

    #[test]
    fn resolve_absolute() {
        assert_eq!(
            base().resolve("https://x.org/y").unwrap().to_string(),
            "https://x.org/y"
        );
        assert_eq!(
            base().resolve("//x.org/y").unwrap().to_string(),
            "http://x.org/y"
        );
    }

    #[test]
    fn resolve_paths() {
        let cases = [
            ("g", "http://a/b/c/g"),
            ("./g", "http://a/b/c/g"),
            ("g/", "http://a/b/c/g/"),
            ("/g", "http://a/g"),
            ("g?y", "http://a/b/c/g?y"),
            ("g?y#s", "http://a/b/c/g?y#s"),
            ("g#s", "http://a/b/c/g#s"),
            ("g;h", "http://a/b/c/g;h"),
            ("..", "http://a/b/"),
            ("../g", "http://a/b/g"),
            ("../..", "http://a/"),
            ("../../g", "http://a/g"),
            ("../../../g", "http://a/g"), // ".." above root clamps
            ("/./g", "http://a/g"),
            ("/../g", "http://a/g"),
            ("g/./h", "http://a/b/c/g/h"),
            ("g/../h", "http://a/b/c/h"),
        ];
        for (rel, want) in cases {
            assert_eq!(
                base().resolve(rel).unwrap().to_string(),
                want,
                "rel={rel:?}"
            );
        }
    }

    #[test]
    fn resolve_same_doc() {
        let b = base();
        assert_eq!(b.resolve("#s").unwrap().to_string(), "http://a/b/c/d;p?q#s");
        assert_eq!(b.resolve("?y").unwrap().to_string(), "http://a/b/c/d;p?y");
        assert_eq!(b.resolve("").unwrap().to_string(), "http://a/b/c/d;p?q");
    }
}
