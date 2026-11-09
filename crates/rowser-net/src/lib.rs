//! rowser-net: networking built from scratch on `std` — URL parsing,
//! DEFLATE/zlib/gzip decompression, and HTTP/1.1 fetching over `TcpStream`.
//! No external dependencies; a sibling crate will add TLS for `https`.

mod http;
mod inflate;
mod url;

pub use http::{Response, fetch, http_get};
pub use inflate::{InflateError, inflate, inflate_gzip, inflate_zlib};
pub use url::{Url, UrlError, percent_decode};

#[cfg(test)]
pub(crate) mod testutil;
