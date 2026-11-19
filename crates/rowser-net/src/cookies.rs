//! Session cookies — an in-memory jar implementing the RFC 6265 basics a
//! browser needs: `Set-Cookie` parsing, domain/path/secure matching, and
//! `Cookie` request-header emission.
//!
//! Deliberate simplifications (fine for a session-only jar):
//! * No disk persistence — the jar dies with the process.
//! * `Expires`/`Max-Age` are honored for expiry and deletion, but there is
//!   no eviction policy and no public-suffix list: the only cross-origin
//!   guard beyond the RFC's suffix rule is rejecting `Domain` values that
//!   are single labels (e.g. `Domain=com`) unless they equal the host.
//! * `HttpOnly` and `SameSite` are parsed and stored but not enforced —
//!   `HttpOnly` only gates `document.cookie` (we have no JS bridge here)
//!   and `SameSite` needs a notion of "site" for the initiating request.
//!
//! # Wiring (for rowser-renderer / headshot redirect loops)
//!
//! Redirect following lives in the *caller* (TLS is a sibling crate), so
//! the jar must be held by the caller and each hop must go through
//! [`Session::request_over`], which (1) attaches `Cookie` for the current
//! URL and (2) stores any `Set-Cookie` headers from the response *before*
//! the caller resolves `Location` and re-requests. A `Set-Cookie` on a
//! 302 is how Google's `?ucbcb=1` consent bypass works, so the capture
//! must happen on every hop, not just the final 200.
//!
//! ```ignore
//! let mut session = rowser_net::Session::with_google_consent();
//! for _ in 0..10 {
//!     let resp = match parsed.scheme.as_str() {
//!         "http" => {
//!             let mut tcp = TcpStream::connect((parsed.host.as_str(), parsed.port))?;
//!             session.request_over(&mut tcp, &parsed)?
//!         }
//!         "https" => {
//!             let mut tls = rowser_tls::TlsStream::connect(&parsed.host, parsed.port, tcp)?;
//!             session.request_over(&mut tls, &parsed)?
//!         }
//!         other => return Err(...),
//!     };
//!     // ... existing redirect logic on resp.status / resp.header("location")
//! }
//! ```
//!
//! To seed Google consent by hand instead of `with_google_consent`:
//! `session.seed_cookie(".google.com", "SOCS", "CAI")`.
//!
//! The wiring above is the manual path. [`Session::fetch`] does the same
//! redirect+cookie loop itself — plus keep-alive connection reuse, DNS
//! caching and TLS session resumption — and [`Session::fetch_all`] runs
//! several of those loops concurrently for subresource fetching.

use std::io;
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::conn::{self, ConnPool};
use crate::http::{IO_TIMEOUT, MAX_REDIRECTS, REDIRECT_STATUSES, Response, request_over_with};
use crate::url::Url;

/// One stored cookie. `domain` is normalized (lowercased, no leading dot).
struct Cookie {
    name: String,
    value: String,
    /// Host cookie (`host_only`) → exact match; domain cookie → suffix match.
    domain: String,
    host_only: bool,
    path: String,
    secure: bool,
    #[allow(dead_code)]
    http_only: bool,
    #[allow(dead_code)]
    same_site: Option<String>,
    /// Absolute expiry, epoch seconds. `None` = session cookie.
    expires_at: Option<i64>,
}

/// In-memory cookie jar. Cheap to clone? No — share one per session/tab.
#[derive(Default)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
}

impl CookieJar {
    /// An empty jar.
    pub fn new() -> Self {
        Self::default()
    }

    /// Store every `Set-Cookie` header found in `resp` as if it arrived
    /// for `url`. Call this after EVERY response — including redirects —
    /// before re-requesting the `Location` target.
    pub fn store_response(&mut self, url: &Url, resp: &Response) {
        self.store_headers(url, &resp.headers);
    }

    /// Store every `Set-Cookie` header in a `(name, value)` header list.
    pub fn store_headers(&mut self, url: &Url, headers: &[(String, String)]) {
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("set-cookie") {
                self.store_set_cookie(url, value);
            }
        }
    }

    /// Store one `Set-Cookie` header value (`"a=1; Path=/; Secure"`),
    /// scoped to `url`'s origin. Malformed or foreign-domain cookies are
    /// silently ignored, per the "fail closed" spirit of RFC 6265.
    pub fn store_set_cookie(&mut self, url: &Url, set_cookie: &str) {
        let Some(sc) = parse_set_cookie(set_cookie) else {
            return;
        };
        let host = url.host.as_str();

        // RFC 6265 §5.3 domain handling: absent Domain → host cookie; a
        // present Domain must equal the host or be a parent suffix of it.
        // IP-literal hosts (Url keeps IPv6 brackets) never take a Domain
        // attribute — only exact host cookies.
        let host_is_ip = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok();
        let (domain, host_only) = match sc.domain {
            Some(raw) => {
                let d = raw.trim_start_matches('.').to_ascii_lowercase();
                let ok_suffix = host == d || host.ends_with(&format!(".{d}"));
                // Cheap public-suffix stand-in: never let a site set a
                // cookie on a bare TLD like `com` (real check needs a PSL).
                if d.is_empty() || !ok_suffix || (!d.contains('.') && d != host) || host_is_ip {
                    return;
                }
                (d, false)
            }
            None => (host.to_ascii_lowercase(), true),
        };

        // Path: explicit value must start with '/', else the default-path
        // (request path up to its last '/').
        let path = match sc.path {
            Some(p) if p.starts_with('/') => p,
            _ => default_path(&url.path),
        };

        // Expiry: Max-Age wins over Expires. ≤ now means "delete".
        let now = now_secs() as i64;
        let expires_at = match sc.max_age {
            Some(age) => Some(now.saturating_add(age)),
            None => sc.expires,
        };
        if expires_at.is_some_and(|t| t <= now) {
            self.cookies
                .retain(|c| !(c.name == sc.name && c.domain == domain && c.path == path));
            return;
        }

        let cookie = Cookie {
            name: sc.name,
            value: sc.value,
            domain,
            host_only,
            path,
            secure: sc.secure,
            http_only: sc.http_only,
            same_site: sc.same_site,
            expires_at,
        };
        // Replace on (name, domain, path) — RFC 6265 §5.3 step 11/12.
        match self
            .cookies
            .iter()
            .position(|c| {
                c.name == cookie.name && c.domain == cookie.domain && c.path == cookie.path
            })
        {
            Some(i) => self.cookies[i] = cookie,
            None => self.cookies.push(cookie),
        }
    }

    /// `a=b; c=d` for every cookie matching `url`'s domain, path and
    /// scheme — `None` when nothing matches. Longer (more specific) paths
    /// sort first, per RFC 6265 §5.4.
    pub fn header_for(&self, url: &Url) -> Option<String> {
        let now = now_secs() as i64;
        let host = url.host.as_str();
        let secure_scheme = matches!(url.scheme.as_str(), "https" | "wss");
        let mut m: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|c| c.expires_at.map_or(true, |t| t > now))
            .filter(|c| {
                if c.host_only {
                    host == c.domain
                } else {
                    host == c.domain || host.ends_with(&format!(".{}", c.domain))
                }
            })
            .filter(|c| !c.secure || secure_scheme)
            .filter(|c| path_matches(&url.path, &c.path))
            .collect();
        if m.is_empty() {
            return None;
        }
        m.sort_by(|a, b| b.path.len().cmp(&a.path.len()));
        Some(
            m.iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    /// Insert a cookie directly — for caller-seeded state like Google's
    /// `SOCS=CAI` consent token. `domain` may carry a leading dot; the
    /// cookie is a domain cookie (suffix match) with `Path=/`, non-Secure,
    /// no expiry.
    pub fn seed_cookie(&mut self, domain: &str, name: &str, value: &str) {
        let d = domain.trim_start_matches('.').to_ascii_lowercase();
        if d.is_empty() || name.is_empty() {
            return;
        }
        let cookie = Cookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: d.clone(),
            host_only: false,
            path: "/".to_string(),
            secure: false,
            http_only: false,
            same_site: None,
            expires_at: None,
        };
        match self
            .cookies
            .iter()
            .position(|c| c.name == name && c.domain == d && c.path == "/")
        {
            Some(i) => self.cookies[i] = cookie,
            None => self.cookies.push(cookie),
        }
    }

    /// Number of stored (not-yet-purged) cookies — diagnostics only;
    /// expired-but-not-yet-purged cookies still count.
    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    /// Drop every cookie (logout / "clear site data").
    pub fn clear(&mut self) {
        self.cookies.clear();
    }
}

/// A browsing session: owns a [`CookieJar`] and wraps the request
/// round-trip so cookies ride along transparently. Hold one per
/// tab/profile and reuse it across requests *and* redirects.
///
/// Besides the stream-overriding [`Session::request_over`], the session
/// carries a keep-alive connection pool: [`Session::fetch`] (and the
/// parallel [`Session::fetch_all`]) connect, TLS-handshake (with
/// resumption) and verify once per origin, then reuse connections —
/// the difference between ~450ms and ~1 RTT per repeat request.
#[derive(Default)]
pub struct Session {
    pub jar: CookieJar,
    pool: ConnPool,
}

impl Session {
    /// Empty session.
    pub fn new() -> Self {
        Self::default()
    }

    /// Session pre-seeded with `SOCS=CAI` for `*.google.com` — the
    /// consent cookie that keeps google.com out of the EU consent-wall
    /// redirect (`consent.google.com?...&ucbcb=1` relies on this, or on a
    /// Set-Cookie round-trip we handle anyway). Returns the session so it
    /// can be used as an expression.
    pub fn with_google_consent() -> Self {
        let mut s = Self::new();
        s.seed_cookie(".google.com", "SOCS", "CAI");
        s
    }

    /// Seed a cookie directly, e.g. `seed_cookie(".google.com", "SOCS", "CAI")`.
    pub fn seed_cookie(&mut self, domain: &str, name: &str, value: &str) {
        self.jar.seed_cookie(domain, name, value);
    }

    /// One HTTP/1.1 GET over an already-connected stream (TCP or TLS),
    /// like [`crate::request_over`] but cookie-aware: attaches `Cookie`
    /// for `url` before writing, then stores `Set-Cookie` headers from the
    /// response afterwards. Does NOT follow redirects — keep the caller's
    /// redirect loop, just call this per hop.
    pub fn request_over<S: std::io::Read + std::io::Write>(
        &mut self,
        stream: &mut S,
        url: &Url,
    ) -> io::Result<Response> {
        let cookie = self.jar.header_for(url);
        let extra: Vec<(&str, &str)> = match &cookie {
            Some(v) => vec![("Cookie", v.as_str())],
            None => Vec::new(),
        };
        let resp = request_over_with(stream, url, &extra)?;
        self.jar.store_response(url, &resp);
        Ok(resp)
    }

    /// `GET url` over plain TCP with cookies, following up to 10
    /// redirects (http:// only — TLS callers must run their own loop over
    /// [`Session::request_over`], which they need anyway for `wss`/TLS
    /// setup). Every hop's `Set-Cookie` is captured before following.
    pub fn get(&mut self, url: &str) -> io::Result<Response> {
        let mut parsed =
            Url::parse(url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        for _ in 0..=MAX_REDIRECTS {
            if parsed.scheme != "http" {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("Session::get only supports http:// (got {}://)", parsed.scheme),
                ));
            }
            let mut stream = TcpStream::connect((parsed.host.as_str(), parsed.port))?;
            stream.set_read_timeout(Some(IO_TIMEOUT))?;
            stream.set_write_timeout(Some(IO_TIMEOUT))?;
            let resp = self.request_over(&mut stream, &parsed)?;
            if !REDIRECT_STATUSES.contains(&resp.status) {
                return Ok(resp);
            }
            let location = resp
                .header("location")
                .map(str::trim)
                .map(str::to_owned)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "redirect without Location")
                })?;
            parsed = parsed
                .resolve_redirect(&location)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "too many redirects",
        ))
    }

    /// `GET url` for `http` or `https`, following up to 10 redirects,
    /// with cookies, keep-alive connection reuse and TLS session
    /// resumption — the full-stack fast path.
    ///
    /// https connections verify the certificate chain against the system
    /// roots (a resumed session authenticates via its PSK); failures are
    /// hard `io::Error`s, never silently skipped.
    pub fn fetch(&mut self, url: &str) -> io::Result<Response> {
        let jar = Mutex::new(std::mem::take(&mut self.jar));
        let r = fetch_loop(url, &jar, &mut self.pool);
        self.jar = jar.into_inner().unwrap_or_default();
        r
    }

    /// Like [`Session::fetch`], but reads and stores cookies through
    /// `jar` instead of this session's own — lets several worker
    /// sessions share one cookie store while keeping their private
    /// connection pools (a `Session` is `!Sync`; a pool must not serve
    /// two requests at once).
    pub fn fetch_with(
        &mut self,
        url: &str,
        jar: &std::sync::Mutex<CookieJar>,
    ) -> io::Result<Response> {
        fetch_loop(url, jar, &mut self.pool)
    }

    /// Fetch several URLs concurrently — one scoped worker thread per
    /// URL, each with its own connection pool, all sharing this
    /// session's cookie jar (locked only for jar access, never across
    /// socket I/O). Results come back in the same order as `urls`.
    /// Per-request timeouts bound every worker.
    pub fn fetch_all<S: AsRef<str> + Sync>(&mut self, urls: &[S]) -> Vec<io::Result<Response>> {
        let jar = Mutex::new(std::mem::take(&mut self.jar));
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = urls
                .iter()
                .map(|u| {
                    let jar = &jar;
                    scope.spawn(move || {
                        let mut pool = ConnPool::default();
                        fetch_loop(u.as_ref(), jar, &mut pool)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join().unwrap_or_else(|_| {
                        Err(io::Error::new(io::ErrorKind::Other, "fetch worker panicked"))
                    })
                })
                .collect()
        });
        self.jar = jar.into_inner().unwrap_or_default();
        results
    }

    /// Drop all pooled idle connections and cached TLS tickets (e.g. on
    /// "clear state" or after a network change).
    pub fn close_connections(&mut self) {
        self.pool.clear();
    }
}

/// Redirect-following fetch loop shared by [`Session::fetch`] and
/// [`Session::fetch_all`]: pooled keep-alive connections, cookies taken
/// from / stored into `jar` (locked around access only).
fn fetch_loop(url: &str, jar: &Mutex<CookieJar>, pool: &mut ConnPool) -> io::Result<Response> {
    let mut parsed =
        Url::parse(url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    for _ in 0..=MAX_REDIRECTS {
        if !matches!(parsed.scheme.as_str(), "http" | "https") {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported URL scheme {:?}", parsed.scheme),
            ));
        }
        let cookie_hdr = jar.lock().unwrap().header_for(&parsed);
        let extra: Vec<(&str, &str)> = match &cookie_hdr {
            Some(v) => vec![("Cookie", v.as_str())],
            None => Vec::new(),
        };
        let x = conn::pooled_exchange(pool, &parsed, &extra)?;
        jar.lock().unwrap().store_response(&parsed, &x.response);
        let resp = x.response;
        if !REDIRECT_STATUSES.contains(&resp.status) {
            return Ok(resp);
        }
        let location = resp
            .header("location")
            .map(str::trim)
            .map(str::to_owned)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "redirect without Location")
            })?;
        parsed = parsed
            .resolve_redirect(&location)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "too many redirects",
    ))
}

/// Parsed `Set-Cookie` header, before origin scoping.
struct SetCookie {
    name: String,
    value: String,
    domain: Option<String>,
    path: Option<String>,
    secure: bool,
    http_only: bool,
    same_site: Option<String>,
    max_age: Option<i64>,
    /// Expires already converted to epoch seconds.
    expires: Option<i64>,
}

/// Parse `name=value; Attr=...; Flag` — RFC 6265 §4.1/§5.2. Unknown
/// attributes are ignored; a missing name or `=` rejects the whole header.
fn parse_set_cookie(header: &str) -> Option<SetCookie> {
    let mut parts = header.split(';');
    let pair = parts.next()?;
    let (name, value) = pair.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let mut value = value.trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        value = &value[1..value.len() - 1];
    }
    let mut sc = SetCookie {
        name: name.to_string(),
        value: value.to_string(),
        domain: None,
        path: None,
        secure: false,
        http_only: false,
        same_site: None,
        max_age: None,
        expires: None,
    };
    for attr in parts {
        let attr = attr.trim();
        let (k, v) = match attr.split_once('=') {
            Some((k, v)) => (k.trim(), Some(v.trim())),
            None => (attr, None),
        };
        match k.to_ascii_lowercase().as_str() {
            "domain" => sc.domain = v.map(str::to_string),
            "path" => sc.path = v.map(str::to_string),
            "max-age" => sc.max_age = v.and_then(|s| s.parse::<i64>().ok()),
            "expires" => {
                if let Some(v) = v {
                    sc.expires = parse_cookie_date(v);
                }
            }
            "secure" => sc.secure = true,
            "httponly" => sc.http_only = true,
            "samesite" => sc.same_site = v.map(str::to_string),
            _ => {}
        }
    }
    Some(sc)
}

/// RFC 6265 §5.1.4 default-path: request path up to (not including) the
/// last '/', or "/" when that would be empty.
fn default_path(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => path[..i].to_string(),
    }
}

/// RFC 6265 §5.1.4 path-match: equal, or request path starts with the
/// cookie path ending at a '/' boundary.
fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    if request_path == cookie_path {
        return true;
    }
    match request_path.strip_prefix(cookie_path) {
        Some(rest) => cookie_path.ends_with('/') || rest.starts_with('/'),
        None => false,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// RFC 6265 §5.1.1 cookie-date parser: tokenize on the spec's delimiter
/// set (alnum and ':' are token chars), pick out hh:mm:ss, 1–2-digit day,
/// month name prefix, 2–4-digit year. Missing time defaults to 00:00:00.
fn parse_cookie_date(s: &str) -> Option<i64> {
    let mut day = None;
    let mut month = None;
    let mut year = None;
    let mut hms = None;
    // Delimiter set = anything that isn't alphanumeric or ':' (so
    // "07:28:00" survives as one token), per RFC 6265 §5.1.1.
    for tok in s
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == ':'))
        .filter(|t| !t.is_empty())
    {
        if hms.is_none() && tok.contains(':') {
            let mut it = tok.split(':').filter_map(|p| p.parse::<u32>().ok());
            if let (Some(h), Some(m), Some(sec)) = (it.next(), it.next(), it.next()) {
                hms = Some((h, m, sec));
                continue;
            }
        }
        if day.is_none()
            && tok.len() <= 2
            && tok.bytes().all(|b| b.is_ascii_digit())
        {
            if let Ok(d) = tok.parse::<u32>() {
                day = Some(d);
                continue;
            }
        }
        if month.is_none() {
            if let Some(m) = month_number(tok) {
                month = Some(m);
                continue;
            }
        }
        if year.is_none()
            && (2..=4).contains(&tok.len())
            && tok.bytes().all(|b| b.is_ascii_digit())
        {
            if let Ok(y) = tok.parse::<i64>() {
                year = Some(y);
            }
        }
    }
    let day = day?;
    let month = month?;
    let mut year = year?;
    if !(1..=31).contains(&day) {
        return None;
    }
    if (70..=99).contains(&year) {
        year += 1900;
    } else if (0..=69).contains(&year) {
        year += 2000;
    }
    let (h, m, sec) = hms.unwrap_or((0, 0, 0));
    if h > 23 || m > 59 || sec > 60 {
        return None;
    }
    Some(
        days_from_civil(year, month, day) * 86_400
            + h as i64 * 3_600
            + m as i64 * 60
            + sec as i64,
    )
}

/// First three ASCII letters (case-insensitive) → month number.
fn month_number(tok: &str) -> Option<u32> {
    let t = tok.as_bytes();
    if t.len() < 3 {
        return None;
    }
    let key = [t[0].to_ascii_lowercase(), t[1].to_ascii_lowercase(), t[2].to_ascii_lowercase()];
    Some(match &key {
        b"jan" => 1,
        b"feb" => 2,
        b"mar" => 3,
        b"apr" => 4,
        b"may" => 5,
        b"jun" => 6,
        b"jul" => 7,
        b"aug" => 8,
        b"sep" => 9,
        b"oct" => 10,
        b"nov" => 11,
        b"dec" => 12,
        _ => return None,
    })
}

/// Howard Hinnant's civil→days algorithm; days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // [0, 11], Mar=0
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn jar_with(url: &str, set_cookie: &str) -> CookieJar {
        let mut j = CookieJar::new();
        j.store_set_cookie(&self::url(url), set_cookie);
        j
    }

    /// Serve each canned response to one connection, in order; yields the
    /// raw request heads received (mirrors the helper in http.rs tests).
    fn serve(responses: Vec<Vec<u8>>) -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for resp in responses {
                let (mut s, _) = listener.accept().unwrap();
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                loop {
                    let n = s.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    if req.windows(4).any(|w| w == b"\r\n\r\n".as_slice()) {
                        break;
                    }
                }
                tx.send(String::from_utf8_lossy(&req).into_owned()).unwrap();
                s.write_all(&resp).unwrap();
            }
        });
        (port, rx)
    }

    /// Serve `responses` on ONE accepted keep-alive connection (request
    /// heads are still reported per request). A client that opens a
    /// second connection instead of reusing the first stalls the test —
    /// `rx.recv_timeout` bounds that failure mode.
    fn serve_keepalive(responses: Vec<Vec<u8>>) -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            for resp in responses {
                let mut req = Vec::new();
                let mut buf = [0u8; 2048];
                loop {
                    let n = s.read(&mut buf).unwrap();
                    if n == 0 {
                        return; // client closed: done
                    }
                    req.extend_from_slice(&buf[..n]);
                    if req.windows(4).any(|w| w == b"\r\n\r\n".as_slice()) {
                        break;
                    }
                }
                tx.send(String::from_utf8_lossy(&req).into_owned()).unwrap();
                s.write_all(&resp).unwrap();
            }
        });
        (port, rx)
    }

    /// `Session::fetch` reuses the pooled connection across a redirect:
    /// the server accepts exactly one TCP connection for both hops, and
    /// the `Set-Cookie` from the 302 rides along on hop 2.
    #[test]
    fn session_fetch_keepalive_redirect_cookies() {
        let (port, rx) = serve_keepalive(vec![
            b"HTTP/1.1 302 Found\r\nLocation: /final\r\nSet-Cookie: sid=xyz; Path=/\r\nContent-Length: 0\r\n\r\n"
                .to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec(),
        ]);
        let mut s = Session::new();
        let resp = s.fetch(&format!("http://127.0.0.1:{port}/start")).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"ok");
        let req1 = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert!(req1.contains("Connection: keep-alive"), "req1={req1:?}");
        let req2 = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert!(req2.contains("Cookie: sid=xyz\r\n"), "req2={req2:?}");
        assert!(req2.split(' ').nth(1) == Some("/final"), "req2={req2:?}");
    }

    /// Two `Session::fetch` calls to the same origin share the pooled
    /// keep-alive connection (no new TCP accept needed).
    #[test]
    fn session_fetch_reuses_connection() {
        let (port, rx) = serve_keepalive(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\none".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\ntwo".to_vec(),
        ]);
        let mut s = Session::new();
        let r1 = s.fetch(&format!("http://127.0.0.1:{port}/a")).unwrap();
        let r2 = s.fetch(&format!("http://127.0.0.1:{port}/b")).unwrap();
        assert_eq!(r1.body, b"one");
        assert_eq!(r2.body, b"two");
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap().split(' ').nth(1), Some("/a"));
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap().split(' ').nth(1), Some("/b"));
    }

    /// A response that says `Connection: close` is not reused — the next
    /// fetch opens a fresh connection.
    #[test]
    fn session_fetch_close_not_reused() {
        // Keep-alive framing for hop 1 (but server says close), then a
        // separate connection for hop 2 (plain serve: one conn each).
        let (port, rx) = serve(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\ny".to_vec(),
        ]);
        let mut s = Session::new();
        let r1 = s.fetch(&format!("http://127.0.0.1:{port}/a")).unwrap();
        let r2 = s.fetch(&format!("http://127.0.0.1:{port}/b")).unwrap();
        assert_eq!(r1.body, b"x");
        assert_eq!(r2.body, b"y");
        // Both requests must have arrived (each on its own connection).
        let _ = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        let _ = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    }

    /// `fetch_all` runs workers concurrently and keeps result order
    /// aligned with the input URLs (the server answers connections in
    /// accept order, so only the response's own URL is asserted).
    #[test]
    fn session_fetch_all_parallel_and_ordered() {
        let (port, _rx) = serve(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec(),
        ]);
        let mut s = Session::new();
        let urls = vec![
            format!("http://127.0.0.1:{port}/a"),
            format!("http://127.0.0.1:{port}/b"),
        ];
        let res = s.fetch_all(&urls);
        assert_eq!(res.len(), 2);
        let r0 = res[0].as_ref().unwrap();
        let r1 = res[1].as_ref().unwrap();
        assert_eq!(r0.body, b"ok");
        assert_eq!(r1.body, b"ok");
        assert!(r0.url.ends_with("/a"), "url={}", r0.url);
        assert!(r1.url.ends_with("/b"), "url={}", r1.url);
    }

    #[test]
    fn host_cookie_exact_match() {
        let j = jar_with("http://example.com/", "a=1");
        assert_eq!(j.header_for(&url("http://example.com/")), Some("a=1".into()));
        // Host cookies do NOT leak to subdomains or parents.
        assert_eq!(j.header_for(&url("http://www.example.com/")), None);
        assert_eq!(j.header_for(&url("http://other.com/")), None);
    }

    #[test]
    fn domain_cookie_suffix_match() {
        let j = jar_with("http://www.example.com/", "a=1; Domain=.example.com");
        assert_eq!(
            j.header_for(&url("http://example.com/")),
            Some("a=1".into())
        );
        assert_eq!(
            j.header_for(&url("http://deep.www.example.com/")),
            Some("a=1".into())
        );
        assert_eq!(j.header_for(&url("http://notexample.com/")), None);
        assert_eq!(j.header_for(&url("http://example.com.evil.net/")), None);
    }

    #[test]
    fn foreign_and_tld_domains_rejected() {
        let mut j = CookieJar::new();
        let u = url("http://www.example.com/");
        // Parent domain is allowed.
        j.store_set_cookie(&u, "ok=1; Domain=example.com");
        // Unrelated domain: rejected.
        j.store_set_cookie(&u, "bad=1; Domain=evil.com");
        // Bare TLD: rejected (host suffix-matches ".com" but we guard it).
        j.store_set_cookie(&u, "tld=1; Domain=com");
        // Sibling is not a suffix of the host: rejected.
        j.store_set_cookie(&u, "sib=1; Domain=other.example.com");
        assert_eq!(j.len(), 1);
        assert_eq!(
            j.header_for(&url("http://anything.example.com/")),
            Some("ok=1".into())
        );
    }

    #[test]
    fn path_scoping_and_default_path() {
        let mut j = CookieJar::new();
        // Set from /a/b/c → default-path /a/b.
        j.store_set_cookie(&url("http://h.com/a/b/c"), "d=1");
        // Explicit path.
        j.store_set_cookie(&url("http://h.com/x"), "p=1; Path=/a");
        // "/a/b/deep" matches BOTH: d (path "/a/b", rest "/deep") and p
        // (path "/a", rest "/b/deep") — both end at a '/' boundary.
        // Longer path sorts first (RFC 6265 §5.4).
        assert_eq!(
            j.header_for(&url("http://h.com/a/b/deep")),
            Some("d=1; p=1".into())
        );
        // "/abc" does not match cookie path "/a" (rest "bc" lacks '/').
        assert_eq!(j.header_for(&url("http://h.com/abc")), None);
        // "/" matches nothing here (no /-path cookie stored).
        assert_eq!(j.header_for(&url("http://h.com/")), None);
    }

    #[test]
    fn secure_only_over_https() {
        let j = jar_with("https://h.com/", "s=1; Secure");
        assert_eq!(j.header_for(&url("https://h.com/x")), Some("s=1".into()));
        assert_eq!(j.header_for(&url("http://h.com/x")), None);
    }

    #[test]
    fn replace_and_delete() {
        let mut j = jar_with("http://h.com/", "a=1");
        j.store_set_cookie(&url("http://h.com/"), "a=2");
        assert_eq!(j.header_for(&url("http://h.com/")), Some("a=2".into()));
        assert_eq!(j.len(), 1);
        // Max-Age=0 deletes.
        j.store_set_cookie(&url("http://h.com/"), "a=gone; Max-Age=0");
        assert!(j.is_empty());
        // Past Expires deletes.
        j.store_set_cookie(&url("http://h.com/"), "b=1");
        j.store_set_cookie(
            &url("http://h.com/"),
            "b=gone; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
        );
        assert!(j.is_empty());
        // Future Expires keeps.
        j.store_set_cookie(
            &url("http://h.com/"),
            "c=1; Expires=Wed, 01 Jan 2098 00:00:00 GMT",
        );
        assert_eq!(j.header_for(&url("http://h.com/")), Some("c=1".into()));
    }

    #[test]
    fn header_format_and_multiple_cookies() {
        let mut j = jar_with("http://h.com/", "a=1");
        j.store_set_cookie(&url("http://h.com/"), "b=two; Path=/");
        assert_eq!(
            j.header_for(&url("http://h.com/")).unwrap(),
            "a=1; b=two"
        );
    }

    #[test]
    fn cookie_date_parsing() {
        // RFC 1123, RFC 850-ish, and asctime shapes.
        let t = parse_cookie_date("Wed, 21 Oct 2015 07:28:00 GMT").unwrap();
        // 2015-10-21 07:28:00 UTC = 1445412480
        assert_eq!(t, 1_445_412_480);
        assert_eq!(
            parse_cookie_date("Sunday, 06-Nov-94 08:49:37 GMT"),
            parse_cookie_date("Sun Nov  6 08:49:37 1994")
        );
        // Two-digit year rules: 94 → 1994, 20 → 2020.
        let t94 = parse_cookie_date("06 Nov 94 08:49:37").unwrap();
        let t20 = parse_cookie_date("06 Nov 20 08:49:37").unwrap();
        assert!(t20 > t94);
        assert_eq!(parse_cookie_date("garbage"), None);
    }

    #[test]
    fn seed_cookie_is_domain_scoped() {
        let mut j = CookieJar::new();
        j.seed_cookie(".google.com", "SOCS", "CAI");
        assert_eq!(
            j.header_for(&url("https://www.google.com/")),
            Some("SOCS=CAI".into())
        );
        assert_eq!(j.header_for(&url("https://google.com/")), Some("SOCS=CAI".into()));
        assert_eq!(j.header_for(&url("https://notgoogle.com/")), None);
    }

    #[test]
    fn session_consent_seed() {
        let s = Session::with_google_consent();
        let h = s.jar.header_for(&url("https://consent.google.com/")).unwrap();
        assert!(h.contains("SOCS=CAI"), "h={h}");
    }

    #[test]
    fn redirect_set_cookie_sent_on_next_hop() {
        // Hop 1: 302 + Set-Cookie. Hop 2 must carry `Cookie: sid=xyz` —
        // this is exactly the consent-redirect pattern the jar exists for.
        let (port, rx) = serve(vec![
            b"HTTP/1.1 302 Found\r\nLocation: /final\r\nSet-Cookie: sid=xyz; Path=/\r\nContent-Length: 0\r\n\r\n"
                .to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec(),
        ]);
        let mut s = Session::new();
        let resp = s.get(&format!("http://127.0.0.1:{port}/start")).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"ok");
        let req1 = rx.recv().unwrap();
        assert!(!req1.contains("Cookie:"), "req1={req1:?}");
        let req2 = rx.recv().unwrap();
        assert!(req2.contains("Cookie: sid=xyz\r\n"), "req2={req2:?}");
    }

    #[test]
    fn cookie_scoped_per_host_over_the_wire() {
        // The Set-Cookie stored for host A must not appear in a request to
        // host B — simulate with two sequential requests through one jar.
        let j = jar_with("http://127.0.0.1/", "a=1");
        assert_eq!(j.header_for(&url("http://127.0.0.1:9/")), Some("a=1".into()));
        assert_eq!(j.header_for(&url("http://127.0.0.2/")), None);
    }

    #[test]
    fn malformed_headers_ignored() {
        let mut j = CookieJar::new();
        let u = url("http://h.com/");
        j.store_set_cookie(&u, "");
        j.store_set_cookie(&u, "=novalue");
        j.store_set_cookie(&u, "noequalsign");
        j.store_set_cookie(&u, "ok=1; Domain=");
        assert_eq!(j.len(), 0);
        j.store_set_cookie(&u, "empty="); // empty value is legal
        assert_eq!(j.header_for(&u), Some("empty=".into()));
    }
}
