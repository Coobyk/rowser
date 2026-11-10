//! rowser-net: networking built from scratch on `std` — URL parsing,
//! DEFLATE/zlib/gzip decompression, and HTTP/1.1 fetching over `TcpStream`.
//! No external dependencies; a sibling crate will add TLS for `https`.

mod http;
mod url;

pub use http::request_over;
pub use http::{Response, fetch, http_get};
pub use rowser_deflate::{InflateError, inflate, inflate_gzip, inflate_zlib};
pub use url::{Url, UrlError, percent_decode};
