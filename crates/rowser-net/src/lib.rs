//! rowser-net: networking built from scratch on `std` — URL parsing,
//! DEFLATE/zlib/gzip decompression, HTTP/1.1 fetching over `TcpStream`,
//! and a session cookie jar. `https` runs over the sibling `rowser-tls`
//! `TlsStream`. For page loads, prefer [`Session::fetch`] /
//! [`Session::fetch_all`]: they pool keep-alive connections per origin
//! and resume TLS sessions instead of paying a handshake per request.

mod conn;
mod cookies;
mod http;
mod url;

pub use cookies::{CookieJar, Session};
pub use http::request_over;
pub use http::{Exchange, Response, exchange, fetch, http_get, request_over_with};
pub use rowser_deflate::{InflateError, inflate, inflate_gzip, inflate_zlib};
pub use url::{Url, UrlError, percent_decode};
