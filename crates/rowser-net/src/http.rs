//! HTTP/1.1 GET: request writing, framed response reading (chunked /
//! Content-Length / close-delimited) and gzip content-decoding, over any
//! `Read + Write` stream — plain TCP or the `rowser-tls` `TlsStream`.
//! [`exchange`] is the keep-alive variant the `Session` connection pool
//! (`crate::conn`) uses; `fetch` is the stateless scheme dispatcher.

use std::io;
use std::net::TcpStream;
use std::time::Duration;

use crate::url::{Url, default_port};
use rowser_deflate::inflate_gzip;

/// Redirect statuses we follow via `Location`, per fetch semantics.
pub(crate) const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];
pub(crate) const MAX_REDIRECTS: usize = 10;
pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// A fetched HTTP response. `body` is already dechunked and content-decoded.
#[derive(Debug, Clone)]
pub struct Response {
    /// Final URL after redirects.
    pub url: String,
    /// Numeric status code, e.g. 200.
    pub status: u16,
    /// `(name, value)` pairs; names lowercased, original order kept.
    pub headers: Vec<(String, String)>,
    /// Decoded response body.
    pub body: Vec<u8>,
}

impl Response {
    /// First header value for `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Body as lossy UTF-8 text.
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// `charset` parameter of the Content-Type header, lowercased.
    pub fn charset(&self) -> Option<String> {
        let ct = self.header("content-type")?;
        for part in ct.split(';').skip(1) {
            let mut kv = part.trim().splitn(2, '=');
            if kv
                .next()
                .is_some_and(|k| k.trim().eq_ignore_ascii_case("charset"))
            {
                if let Some(v) = kv.next() {
                    let v = v.trim().trim_matches('"').trim_matches('\'');
                    if !v.is_empty() {
                        return Some(v.to_ascii_lowercase());
                    }
                }
            }
        }
        None
    }
}

/// `GET url` over HTTP/1.1, following up to 10 redirects.
/// Errors on non-`http` schemes — use [`fetch`] for dispatch.
pub fn http_get(url: &str) -> io::Result<Response> {
    let mut parsed = Url::parse(url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    for _ in 0..=MAX_REDIRECTS {
        let resp = single_request(&parsed)?;
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

/// Scheme-dispatching fetch (stateless: no cookies, no pooling).
/// `https` is unsupported here — use [`crate::Session::fetch`], which
/// brings its own TLS, verification, keep-alive pool and cookie jar.
pub fn fetch(url: &str) -> io::Result<Response> {
    let parsed = Url::parse(url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    match parsed.scheme.as_str() {
        "http" => http_get(url),
        "https" => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "https is handled by Session::fetch (needs the TLS pool)",
        )),
        other => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("unsupported URL scheme {other:?}"),
        )),
    }
}

/// One HTTP/1.1 round-trip: connect, send request, read to close, parse.
fn single_request(url: &Url) -> io::Result<Response> {
    if url.scheme != "http" {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("http_get only supports http:// (got {}://)", url.scheme),
        ));
    }
    let mut stream = TcpStream::connect((url.host.as_str(), url.port))?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    request_over(&mut stream, url)
}

/// One HTTP/1.1 GET over an already-connected stream (plain TCP or TLS).
/// Sends `Connection: close` — for keep-alive reuse see [`exchange`].
/// Does NOT follow redirects — inspect `Response.status` + `location`.
pub fn request_over<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    url: &Url,
) -> io::Result<Response> {
    request_over_with(stream, url, &[])
}

/// [`request_over`] plus caller-supplied extra request headers — the seam
/// [`crate::Session`] uses to attach `Cookie` headers. `(name, value)` pairs
/// are emitted verbatim as `name: value`; pairs containing CR/LF are skipped
/// to keep header injection out. Sends `Connection: close`.
pub fn request_over_with<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    url: &Url,
    extra_headers: &[(&str, &str)],
) -> io::Result<Response> {
    round_trip(stream, url, extra_headers, Vec::new(), /* keep_alive */ false)
        .map(|x| x.response)
}

/// The wire-level result of one request: the response plus connection
/// state a keep-alive pool needs.
#[derive(Debug)]
pub struct Exchange {
    /// The parsed, decoded response.
    pub response: Response,
    /// True when the connection may carry another request: the server
    /// did not say `Connection: close` and the body was fully delimited
    /// by `Content-Length`/`chunked` (not by connection close).
    pub keep_alive: bool,
    /// Bytes read past the end of this response (essentially always
    /// empty — we never pipeline). Feed back as the next `buffered`.
    pub leftover: Vec<u8>,
}

/// One HTTP/1.1 GET with `Connection: keep-alive`: the response body is
/// delimited by its framing (chunked / Content-Length / close), so the
/// stream stays usable for the next request when `Exchange.keep_alive`.
/// `buffered` carries any leftover bytes from a previous [`Exchange`] on
/// the same connection; pass `Vec::new()` on a fresh stream.
pub fn exchange<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    url: &Url,
    extra_headers: &[(&str, &str)],
    buffered: Vec<u8>,
) -> io::Result<Exchange> {
    round_trip(stream, url, extra_headers, buffered, /* keep_alive */ true)
}

/// Cap on response-head size — generous (big sites send many KB of
/// headers) but finite so a runaway peer can't grow memory unboundedly.
const MAX_HEAD: usize = 256 * 1024;
/// Cap on a single chunk-size or trailer line.
const MAX_LINE: usize = 64 * 1024;

fn round_trip<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    url: &Url,
    extra_headers: &[(&str, &str)],
    buffered: Vec<u8>,
    keep_alive: bool,
) -> io::Result<Exchange> {
    let host_header = if default_port(&url.scheme) == Some(url.port) {
        url.host.clone()
    } else {
        format!("{}:{}", url.host, url.port)
    };
    let mut request = format!(
        "GET {} HTTP/1.1\r\nHost: {host_header}\r\n\
         User-Agent: Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36\r\n\
         Accept: */*\r\n\
         Accept-Encoding: gzip\r\n",
        url.request_target()
    );
    for (name, value) in extra_headers {
        let clean = |s: &str| !s.bytes().any(|b| b == b'\r' || b == b'\n');
        if clean(name) && clean(value) {
            request.push_str(name);
            request.push_str(": ");
            request.push_str(value);
            request.push_str("\r\n");
        }
    }
    request.push_str(if keep_alive {
        "Connection: keep-alive\r\n\r\n"
    } else {
        "Connection: close\r\n\r\n"
    });
    stream.write_all(request.as_bytes())?;

    let mut buf = buffered;
    // Skip 1xx informational heads (100 Continue, 103 Early Hints…) —
    // bounded so a hostile peer can't stream them forever.
    let mut head = None;
    for _ in 0..8 {
        let h = read_head(stream, &mut buf)?;
        if !(100..200).contains(&h.status) || h.status == 101 {
            head = Some(h);
            break;
        }
    }
    let head = head.ok_or_else(|| invalid_data("too many interim responses"))?;

    let get = |name: &str| -> Option<&str> {
        head.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };

    // RFC 9112 §6.3 body framing.
    let no_body = head.status == 101
        || (100..200).contains(&head.status)
        || head.status == 204
        || head.status == 304;
    let chunked = get("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let content_len: Option<usize> = get("content-length").and_then(|v| v.trim().parse().ok());

    let (mut body, delimited) = if no_body {
        (Vec::new(), true)
    } else if chunked {
        (read_chunked(stream, &mut buf)?, true)
    } else if let Some(len) = content_len {
        fill(stream, &mut buf, len)?;
        let b = buf[..len].to_vec();
        buf.drain(..len);
        (b, true)
    } else {
        // No length: body runs to connection close.
        let mut b = std::mem::take(&mut buf);
        stream.read_to_end(&mut b)?;
        (b, false)
    };

    if get("content-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("gzip")) {
        body = inflate_gzip(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    }

    // Reusable iff the body didn't end at close AND the connection stays
    // open: HTTP/1.1 defaults persistent; 1.0 needs an explicit token.
    let conn = get("connection").unwrap_or_default().to_ascii_lowercase();
    let said_close = conn.split(',').any(|t| t.trim() == "close");
    let said_keep = conn.split(',').any(|t| t.trim() == "keep-alive");
    let keep = delimited && !said_close && (!head.legacy_10 || said_keep);

    Ok(Exchange {
        response: Response {
            url: url.to_string(),
            status: head.status,
            headers: head.headers,
            body,
        },
        keep_alive: keep,
        leftover: buf,
    })
}

/// Parsed response head.
struct Head {
    status: u16,
    /// Peer spoke HTTP/1.0 — needs explicit `Connection: keep-alive`.
    legacy_10: bool,
    headers: Vec<(String, String)>,
}

/// Read (buffered `buf` first, then `stream`) until `\r\n\r\n`, parse the
/// status line and headers, and drain them from `buf`.
fn read_head<S: std::io::Read>(stream: &mut S, buf: &mut Vec<u8>) -> io::Result<Head> {
    let mut chunk = [0u8; 16 * 1024];
    let head_end = loop {
        if let Some(i) = find_subslice(buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() >= MAX_HEAD {
            return Err(invalid_data("response head too large"));
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before response head",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = std::str::from_utf8(&buf[..head_end])
        .map_err(|_| invalid_data("response head is not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        return Err(invalid_data("malformed status line"));
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| invalid_data("malformed status code"))?;
    let legacy_10 = version == "HTTP/1.0";
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    buf.drain(..head_end + 4);
    Ok(Head {
        status,
        legacy_10,
        headers,
    })
}

/// Ensure `buf` holds at least `need` bytes, reading more from `stream`.
fn fill<S: std::io::Read>(stream: &mut S, buf: &mut Vec<u8>, need: usize) -> io::Result<()> {
    let mut chunk = [0u8; 16 * 1024];
    while buf.len() < need {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed mid-response",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(())
}

/// Read one CRLF-terminated line out of `buf`/`stream`, returning it
/// without the terminator and draining it.
fn read_line<S: std::io::Read>(stream: &mut S, buf: &mut Vec<u8>) -> io::Result<Vec<u8>> {
    let mut chunk = [0u8; 16 * 1024];
    loop {
        if let Some(i) = find_subslice(buf, b"\r\n") {
            let line = buf[..i].to_vec();
            buf.drain(..i + 2);
            return Ok(line);
        }
        if buf.len() >= MAX_LINE {
            return Err(invalid_data("chunk line too large"));
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed mid-chunk",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Read a `Transfer-Encoding: chunked` body off the wire, dechunking as
/// we go; trailers are skipped. Bytes past the terminating chunk stay in
/// `buf` so the connection stays usable.
fn read_chunked<S: std::io::Read>(stream: &mut S, buf: &mut Vec<u8>) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line = read_line(stream, buf)?;
        let size_str = std::str::from_utf8(&line)
            .map_err(|_| invalid_data("chunk size not ASCII"))?;
        // Chunk extensions ("size;key=val") are ignored.
        let size_str = size_str.split(';').next().unwrap_or_default().trim();
        let size =
            usize::from_str_radix(size_str, 16).map_err(|_| invalid_data("bad chunk size"))?;
        if size == 0 {
            // Trailer section ends at a blank line.
            loop {
                let t = read_line(stream, buf)?;
                if t.is_empty() {
                    return Ok(out);
                }
            }
        }
        fill(stream, buf, size + 2)?;
        if buf[size] != b'\r' || buf[size + 1] != b'\n' {
            return Err(invalid_data("chunk missing CRLF terminator"));
        }
        out.extend_from_slice(&buf[..size]);
        buf.drain(..size + 2);
    }
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn invalid_data(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rowser_deflate::testutil;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    /// Serve each canned response to one connection, in order. Returns the
    /// port plus a channel that yields the raw request heads received.
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
                    if find_subslice(&req, b"\r\n\r\n").is_some() {
                        break;
                    }
                }
                tx.send(String::from_utf8_lossy(&req).into_owned()).unwrap();
                s.write_all(&resp).unwrap();
            }
        });
        (port, rx)
    }

    #[test]
    fn plain_get() {
        let (port, rx) = serve(vec![
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello"
                .to_vec(),
        ]);
        let resp = http_get(&format!("http://127.0.0.1:{port}/some/path?x=1")).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"hello");
        assert_eq!(resp.header("Content-Type"), Some("text/plain"));
        assert_eq!(resp.body_text(), "hello");

        let req = rx.recv().unwrap();
        assert!(
            req.starts_with("GET /some/path?x=1 HTTP/1.1\r\n"),
            "req={req:?}"
        );
        assert!(
            req.contains(&format!("Host: 127.0.0.1:{port}")),
            "req={req:?}"
        );
        assert!(req.contains("Accept-Encoding: gzip"), "req={req:?}");
        assert!(req.contains("Connection: close"), "req={req:?}");
    }

    #[test]
    fn chunked_gzip_body() {
        let payload = b"hello hello hello, compressed chunked world!";
        let gz = testutil::gzip_dynamic(payload);
        // Split the gzip stream into several small chunks.
        let mut body = Vec::new();
        for chunk in gz.chunks(7) {
            body.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            body.extend_from_slice(chunk);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(b"0\r\n\r\n");
        let mut raw =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Encoding: gzip\r\n\r\n"
                .to_vec();
        raw.extend_from_slice(&body);

        let (port, _rx) = serve(vec![raw]);
        let resp = http_get(&format!("http://127.0.0.1:{port}/")).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, payload);
    }

    #[test]
    fn redirect_followed() {
        let (port, rx) = serve(vec![
            b"HTTP/1.1 301 Moved Permanently\r\nLocation: /final\r\nContent-Length: 0\r\n\r\n"
                .to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndone".to_vec(),
        ]);
        let resp = http_get(&format!("http://127.0.0.1:{port}/start")).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"done");
        assert_eq!(resp.url, format!("http://127.0.0.1:{port}/final"));
        assert_eq!(rx.recv().unwrap().split(' ').nth(1), Some("/start"));
        assert_eq!(rx.recv().unwrap().split(' ').nth(1), Some("/final"));
    }

    #[test]
    fn redirect_loop_errors() {
        let raw = b"HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\n\r\n".to_vec();
        let (port, _rx) = serve(vec![raw; MAX_REDIRECTS + 1]);
        let err = http_get(&format!("http://127.0.0.1:{port}/again")).unwrap_err();
        assert_eq!(err.to_string(), "too many redirects");
    }

    #[test]
    fn fetch_dispatches_on_scheme() {
        let (port, _rx) = serve(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec(),
        ]);
        let resp = fetch(&format!("http://127.0.0.1:{port}/")).unwrap();
        assert_eq!(resp.body, b"ok");

        let err = fetch("https://example.com/").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        let err = fetch("gopher://example.com/").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }
}
