//! Keep-alive connection pool and DNS resolution cache backing
//! [`crate::Session::fetch`] / [`crate::Session::fetch_all`].
//!
//! A browser page load repeats requests to the same origins; paying a
//! TCP + TLS 1.3 handshake (2 RTTs plus certificate verification) per
//! request dominated cold-load time. `ConnPool` keeps idle connections
//! per `(scheme, host, port)` and hands them out for reuse when the
//! previous exchange ended cleanly on a message boundary
//! (`Exchange.keep_alive`). Resumption tickets harvested from verified
//! TLS sessions are offered on reconnect so even a fresh connection to a
//! seen origin can skip most of the handshake.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};

use rowser_tls::{ResumptionTicket, TlsStream};

use crate::http::{Exchange, IO_TIMEOUT, exchange};
use crate::url::Url;

/// `(scheme, host, port)` — connections are never shared across origins.
pub(crate) type Origin = (String, String, u16);

pub(crate) fn origin(url: &Url) -> Origin {
    (url.scheme.clone(), url.host.clone(), url.port)
}

/// Max idle connections kept per origin.
const MAX_IDLE_PER_ORIGIN: usize = 4;

/// A connected stream: plain TCP or TLS. Both expose `Read`/`Write`, so
/// [`exchange`] works over either.
pub(crate) enum Stream {
    Tcp(TcpStream),
    Tls(TlsStream),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Tcp(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// An idle pooled connection plus any bytes read ahead of the next
/// response (almost always empty).
pub(crate) struct Idle {
    pub stream: Stream,
    pub buffered: Vec<u8>,
}

/// Per-origin idle connections + resumption tickets. One pool per
/// worker thread in `fetch_all` — pools are `!Sync` by design (a
/// connection must not serve two requests at once).
#[derive(Default)]
pub(crate) struct ConnPool {
    idle: HashMap<Origin, Vec<Idle>>,
    tickets: HashMap<Origin, ResumptionTicket>,
}

impl ConnPool {
    /// A connection for `url`: an idle pooled one when available
    /// (`reused == true`), else a freshly connected one.
    pub fn connection(&mut self, url: &Url) -> io::Result<(Idle, bool)> {
        let o = origin(url);
        if let Some(v) = self.idle.get_mut(&o) {
            if let Some(idle) = v.pop() {
                return Ok((idle, true));
            }
        }
        Ok((
            Idle {
                stream: self.connect(url, &o)?,
                buffered: Vec::new(),
            },
            false,
        ))
    }

    /// A guaranteed-fresh connection (retry path for a stale pooled one).
    pub fn fresh(&mut self, url: &Url) -> io::Result<Idle> {
        Ok(Idle {
            stream: self.connect(url, &origin(url))?,
            buffered: Vec::new(),
        })
    }

    /// Return a still-usable connection to the pool.
    pub fn put(&mut self, url: &Url, conn: Idle) {
        let o = origin(url);
        let v = self.idle.entry(o).or_default();
        if v.len() < MAX_IDLE_PER_ORIGIN {
            v.push(conn);
        }
    }

    /// Stash any resumption tickets a TLS stream produced during reads.
    /// Only call for sessions whose certificate verified (or which were
    /// themselves resumed from a verified session).
    pub fn harvest_tickets(&mut self, url: &Url, stream: &mut Stream) {
        if let Stream::Tls(tls) = stream {
            for t in tls.take_resumption_tickets() {
                self.tickets.insert(origin(url), t);
            }
        }
    }

    /// Drop all idle connections and tickets.
    pub fn clear(&mut self) {
        self.idle.clear();
        self.tickets.clear();
    }

    fn connect(&mut self, url: &Url, o: &Origin) -> io::Result<Stream> {
        match url.scheme.as_str() {
            "http" => Ok(Stream::Tcp(connect_tcp(&url.host, url.port)?)),
            "https" => {
                let ticket = self.tickets.get(o).filter(|t| !t.expired()).cloned();
                let tls = match ticket {
                    Some(t) => {
                        match TlsStream::connect_resumed(
                            &url.host,
                            url.port,
                            connect_tcp(&url.host, url.port)?,
                            &t,
                        ) {
                            Ok(s) => s,
                            // Resumption can fail mid-handshake (expired
                            // ticket, picky middlebox); fall back to a
                            // full handshake on a new socket.
                            Err(_) => TlsStream::connect(
                                &url.host,
                                url.port,
                                connect_tcp(&url.host, url.port)?,
                            )?,
                        }
                    }
                    None => {
                        TlsStream::connect(&url.host, url.port, connect_tcp(&url.host, url.port)?)?
                    }
                };
                // A resumed session authenticated via the PSK from a
                // previously verified handshake; anything else must
                // present a valid chain. No downgrade here.
                if !tls.resumed() {
                    tls.verify_certificate().map_err(io::Error::from)?;
                }
                Ok(Stream::Tls(tls))
            }
            other => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported URL scheme {other:?}"),
            )),
        }
    }
}

/// Run one request on a pooled connection, then return it to `pool` if
/// the connection is still usable. When the connection came from the
/// pool and the exchange fails, retries once on a fresh connection — an
/// idle pooled socket may have been closed server-side since checkout.
pub(crate) fn pooled_exchange(
    pool: &mut ConnPool,
    url: &Url,
    extra_headers: &[(&str, &str)],
) -> io::Result<Exchange> {
    let (mut idle, reused) = pool.connection(url)?;
    let buffered = std::mem::take(&mut idle.buffered);
    let mut x = match exchange(&mut idle.stream, url, extra_headers, buffered) {
        Ok(x) => x,
        Err(e) => {
            if !reused {
                return Err(e);
            }
            let mut fresh = pool.fresh(url)?;
            match exchange(&mut fresh.stream, url, extra_headers, Vec::new()) {
                Ok(x) => {
                    idle = fresh;
                    x
                }
                Err(e) => return Err(e),
            }
        }
    };
    pool.harvest_tickets(url, &mut idle.stream);
    if x.keep_alive {
        idle.buffered = std::mem::take(&mut x.leftover);
        pool.put(url, idle);
    }
    Ok(x)
}

/// Cached `getaddrinfo` per `host:port` — DNS answers outlive a page
/// load easily, and repeat connects otherwise pay a lookup each time.
/// Stale entries self-heal: a failed connect retries once with a fresh
/// resolution.
fn dns_cache() -> &'static Mutex<HashMap<String, Vec<SocketAddr>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Vec<SocketAddr>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn resolve(host: &str, port: u16, refresh: bool) -> io::Result<Vec<SocketAddr>> {
    // Url keeps IPv6 literals bracketed; resolution wants them bare.
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let key = format!("{bare}:{port}");
    if !refresh {
        if let Some(addrs) = dns_cache().lock().unwrap().get(&key) {
            if !addrs.is_empty() {
                return Ok(addrs.clone());
            }
        }
    }
    let addrs: Vec<SocketAddr> = (bare, port).to_socket_addrs()?.collect();
    if !addrs.is_empty() {
        dns_cache().lock().unwrap().insert(key, addrs.clone());
    }
    Ok(addrs)
}

/// TCP connect with timeouts, DNS caching, and nodelay — the standard
/// connection every pooled stream starts from.
pub(crate) fn connect_tcp(host: &str, port: u16) -> io::Result<TcpStream> {
    let mut last_err = io::Error::new(io::ErrorKind::Other, "no resolved addresses");
    // Pass 1 uses (possibly cached) addrs; on total failure, evict and
    // re-resolve once in case the cached addresses went stale.
    for attempt in 0..2 {
        let addrs = resolve(host, port, attempt == 1)?;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, IO_TIMEOUT) {
                Ok(s) => {
                    s.set_read_timeout(Some(IO_TIMEOUT))?;
                    s.set_write_timeout(Some(IO_TIMEOUT))?;
                    let _ = s.set_nodelay(true);
                    return Ok(s);
                }
                Err(e) => last_err = e,
            }
        }
        // All addrs failed — loop re-resolves with the cache bypassed, so
        // a stale cached entry can't wedge the origin.
    }
    Err(last_err)
}
