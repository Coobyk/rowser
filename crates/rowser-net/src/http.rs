//! HTTP/1.1 GET over plain TCP (`http://`, port 80). Response dechunking and
//! gzip content-decoding included. TLS for `https://` lands in a sibling
//! crate; `fetch` is the seam where it will plug in.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::inflate::inflate_gzip;
use crate::url::{Url, default_port};

/// Redirect statuses we follow via `Location`, per fetch semantics.
const REDIRECT_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];
const MAX_REDIRECTS: usize = 10;
const IO_TIMEOUT: Duration = Duration::from_secs(30);

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
}

/// `GET url` over HTTP/1.1, following up to 10 redirects.
/// Errors on non-`http` schemes — use [`fetch`] for dispatch.
pub fn http_get(url: &str) -> io::Result<Response> {
    let mut parsed =
        Url::parse(url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
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
            .resolve(&location)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "too many redirects",
    ))
}

/// Scheme-dispatching fetch. `https` is a clean seam: once the TLS sibling
/// crate exists it gets called from here.
pub fn fetch(url: &str) -> io::Result<Response> {
    let parsed =
        Url::parse(url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    match parsed.scheme.as_str() {
        "http" => http_get(url),
        "https" => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "https requires TLS, which is not implemented yet",
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
/// Does NOT follow redirects — inspect `Response.status` + `location`.
pub fn request_over<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    url: &Url,
) -> io::Result<Response> {
    let host_header = if default_port(&url.scheme) == Some(url.port) {
        url.host.clone()
    } else {
        format!("{}:{}", url.host, url.port)
    };
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {host_header}\r\nAccept: */*\r\n\
         Accept-Encoding: gzip\r\nConnection: close\r\n\r\n",
        url.request_target()
    );
    stream.write_all(request.as_bytes())?;

    // `Connection: close` means the body ends at EOF.
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;
    parse_response(&raw, url)
}

/// Split raw response bytes into status line, headers, and decoded body.
fn parse_response(raw: &[u8], url: &Url) -> io::Result<Response> {
    let head_end = find_subslice(raw, b"\r\n\r\n")
        .ok_or_else(|| invalid_data("response missing header terminator"))?;
    let head = std::str::from_utf8(&raw[..head_end])
        .map_err(|_| invalid_data("response head is not UTF-8"))?;

    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    if !parts.next().unwrap_or_default().starts_with("HTTP/") {
        return Err(invalid_data("malformed status line"));
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| invalid_data("malformed status code"))?;

    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let get = |name: &str| -> Option<&str> {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };

    let mut body = raw[head_end + 4..].to_vec();
    if get("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
    {
        body = dechunk(&body)?;
    } else if let Some(len) = get("content-length").and_then(|v| v.parse().ok()) {
        body.truncate(len);
    }
    if get("content-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("gzip"))
    {
        body = inflate_gzip(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    }

    Ok(Response { url: url.to_string(), status, headers, body })
}

/// Undo `Transfer-Encoding: chunked` (RFC 9112 §7.1); trailers are skipped.
fn dechunk(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        let end = find_subslice(&data[pos..], b"\r\n")
            .map(|i| pos + i)
            .ok_or_else(|| invalid_data("unterminated chunk size"))?;
        let size_str = std::str::from_utf8(&data[pos..end])
            .map_err(|_| invalid_data("chunk size not ASCII"))?;
        // Chunk extensions ("size;key=val") are ignored.
        let size_str = size_str.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| invalid_data("bad chunk size"))?;
        pos = end + 2;
        if size == 0 {
            return Ok(out);
        }
        if pos + size + 2 > data.len() {
            return Err(invalid_data("truncated chunk"));
        }
        out.extend_from_slice(&data[pos..pos + size]);
        pos += size;
        if data[pos..pos + 2] != *b"\r\n" {
            return Err(invalid_data("chunk missing CRLF terminator"));
        }
        pos += 2;
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
    use crate::testutil;
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
        assert!(req.starts_with("GET /some/path?x=1 HTTP/1.1\r\n"), "req={req:?}");
        assert!(req.contains(&format!("Host: 127.0.0.1:{port}")), "req={req:?}");
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
        let raw =
            b"HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\n\r\n".to_vec();
        let (port, _rx) = serve(vec![raw; MAX_REDIRECTS + 1]);
        let err = http_get(&format!("http://127.0.0.1:{port}/again")).unwrap_err();
        assert_eq!(err.to_string(), "too many redirects");
    }

    #[test]
    fn fetch_dispatches_on_scheme() {
        let (port, _rx) =
            serve(vec![b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec()]);
        let resp = fetch(&format!("http://127.0.0.1:{port}/")).unwrap();
        assert_eq!(resp.body, b"ok");

        let err = fetch("https://example.com/").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        let err = fetch("gopher://example.com/").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    }
}
