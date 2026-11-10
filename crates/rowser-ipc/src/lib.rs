//! IPC between the `rowser` browser process and `rowser-renderer` children.
//!
//! Hand-rolled binary codec — no serde. Wire format:
//!
//! ```text
//! [u32 LE payload_len][u8 tag][fields...]
//! ```
//!
//! Strings are `u32 len + utf8 bytes`, `Vec<u8>` is `u32 len + bytes`.
//! Frame pixels are premultiplied BGRA, `width * height * 4` bytes.

use std::io::{self, Read, Write};

/// A cursor over a received payload for decoding fields.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(bad)?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or_else(bad)?;
        self.pos = end;
        Ok(slice)
    }

    pub fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }

    pub fn string(&mut self) -> io::Result<String> {
        let bytes = self.bytes()?;
        String::from_utf8(bytes).map_err(|_| bad())
    }
}

/// Append-only payload builder.
#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn bytes(&mut self, v: &[u8]) {
        self.u32(v.len() as u32);
        self.buf.extend_from_slice(v);
    }

    pub fn string(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }

    pub fn into_payload(self) -> Vec<u8> {
        self.buf
    }
}

fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "malformed ipc message")
}

/// Write a length-prefixed frame to a stream.
pub fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ipc frame too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// Read a length-prefixed frame from a stream.
pub fn read_frame(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Messages sent from the browser process to a renderer.
#[derive(Debug, Clone, PartialEq)]
pub enum ToRenderer {
    Ping(u64),
    /// Navigate to a URL (file://, http(s)://, or a synthetic scheme).
    Navigate(String),
    /// Viewport size changed; the renderer should produce a new frame.
    ResizeViewport { width: u32, height: u32 },
    /// Scroll the document by dy pixels (positive = down).
    Scroll { dy: f32 },
    /// Mouse click at document-space coordinates (device px).
    Click { x: f32, y: f32 },
    /// Text selection drag: anchor (x0,y0) to focus (x1,y1), same
    /// document-space device px as `Click`. Sent on mouse move while the
    /// left button is held; the renderer re-paints with the selected text
    /// highlighted and replies with `ToBrowser::SelText`.
    Select { x0: f32, y0: f32, x1: f32, y1: f32 },
    /// Request a text dump of the DOM tree (inspector).
    DomDump,
    Shutdown,
}

/// Messages sent from a renderer to the browser process.
#[derive(Debug, Clone, PartialEq)]
pub enum ToBrowser {
    Pong(u64),
    LoadStarted,
    /// Document title after navigation completes.
    Title(String),
    /// A newly painted frame: premultiplied BGRA pixels.
    Frame {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
    LoadDone,
    /// Free-form diagnostics from the renderer (shown in a log view later).
    Log(String),
    /// The user clicked a link — browser should navigate.
    NavigationRequest(String),
    /// Indented DOM tree text for the inspector.
    DomDump(String),
    /// Currently selected page text ("" when the selection is empty) —
    /// sent after each `Select` so the browser can copy on Ctrl+C.
    SelText(String),
}

const TAG_PING: u8 = 0;
const TAG_NAVIGATE: u8 = 1;
const TAG_RESIZE: u8 = 2;
const TAG_SCROLL: u8 = 3;
const TAG_CLICK: u8 = 5;
const TAG_DOMDUMP: u8 = 6;
const TAG_SELECT: u8 = 8;
const TAG_SHUTDOWN: u8 = 4;

const TAG_PONG: u8 = 0;
const TAG_LOAD_STARTED: u8 = 1;
const TAG_TITLE: u8 = 2;
const TAG_FRAME: u8 = 3;
const TAG_LOAD_DONE: u8 = 4;
const TAG_LOG: u8 = 5;
const TAG_NAV_REQUEST: u8 = 6;
const TAG_BDOMDUMP: u8 = 7;
const TAG_SELTEXT: u8 = 8;

impl ToRenderer {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Self::Ping(n) => {
                w.u8(TAG_PING);
                w.u64(*n);
            }
            Self::Navigate(url) => {
                w.u8(TAG_NAVIGATE);
                w.string(url);
            }
            Self::ResizeViewport { width, height } => {
                w.u8(TAG_RESIZE);
                w.u32(*width);
                w.u32(*height);
            }
            Self::Scroll { dy } => {
                w.u8(TAG_SCROLL);
                w.u32(dy.to_bits());
            }
            Self::Click { x, y } => {
                w.u8(TAG_CLICK);
                w.u32(x.to_bits());
                w.u32(y.to_bits());
            }
            Self::Select { x0, y0, x1, y1 } => {
                w.u8(TAG_SELECT);
                w.u32(x0.to_bits());
                w.u32(y0.to_bits());
                w.u32(x1.to_bits());
                w.u32(y1.to_bits());
            }
            Self::DomDump => w.u8(TAG_DOMDUMP),
            Self::Shutdown => w.u8(TAG_SHUTDOWN),
        }
        w.into_payload()
    }

    pub fn decode(payload: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(payload);
        Ok(match r.u8()? {
            TAG_PING => Self::Ping(r.u64()?),
            TAG_NAVIGATE => Self::Navigate(r.string()?),
            TAG_RESIZE => Self::ResizeViewport {
                width: r.u32()?,
                height: r.u32()?,
            },
            TAG_SCROLL => Self::Scroll {
                dy: f32::from_bits(r.u32()?)
            },
            TAG_CLICK => Self::Click {
                x: f32::from_bits(r.u32()?) ,
                y: f32::from_bits(r.u32()?),
            },
            TAG_SELECT => Self::Select {
                x0: f32::from_bits(r.u32()?),
                y0: f32::from_bits(r.u32()?),
                x1: f32::from_bits(r.u32()?),
                y1: f32::from_bits(r.u32()?),
            },
            TAG_DOMDUMP => Self::DomDump,
            TAG_SHUTDOWN => Self::Shutdown,
            tag => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown ToRenderer tag {tag}"),
                ))
            }
        })
    }
}

impl ToBrowser {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            Self::Pong(n) => {
                w.u8(TAG_PONG);
                w.u64(*n);
            }
            Self::LoadStarted => w.u8(TAG_LOAD_STARTED),
            Self::Title(title) => {
                w.u8(TAG_TITLE);
                w.string(title);
            }
            Self::Frame {
                width,
                height,
                pixels,
            } => {
                w.u8(TAG_FRAME);
                w.u32(*width);
                w.u32(*height);
                w.bytes(pixels);
            }
            Self::LoadDone => w.u8(TAG_LOAD_DONE),
            Self::Log(msg) => {
                w.u8(TAG_LOG);
                w.string(msg);
            }
            Self::NavigationRequest(url) => {
                w.u8(TAG_NAV_REQUEST);
                w.string(url);
            }
            Self::DomDump(text) => {
                w.u8(TAG_BDOMDUMP);
                w.string(text);
            }
            Self::SelText(text) => {
                w.u8(TAG_SELTEXT);
                w.string(text);
            }
        }
        w.into_payload()
    }

    pub fn decode(payload: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(payload);
        Ok(match r.u8()? {
            TAG_PONG => Self::Pong(r.u64()?),
            TAG_LOAD_STARTED => Self::LoadStarted,
            TAG_TITLE => Self::Title(r.string()?),
            TAG_FRAME => Self::Frame {
                width: r.u32()?,
                height: r.u32()?,
                pixels: r.bytes()?,
            },
            TAG_LOAD_DONE => Self::LoadDone,
            TAG_LOG => Self::Log(r.string()?),
            TAG_NAV_REQUEST => Self::NavigationRequest(r.string()?),
            TAG_BDOMDUMP => Self::DomDump(r.string()?),
            TAG_SELTEXT => Self::SelText(r.string()?),
            tag => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown ToBrowser tag {tag}"),
                ))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn to_renderer_round_trip() {
        let msgs = [
            ToRenderer::Ping(42),
            ToRenderer::Navigate("file:///tmp/page.html".into()),
            ToRenderer::ResizeViewport {
                width: 1920,
                height: 1080,
            },
            ToRenderer::Scroll { dy: -240.5 },
            ToRenderer::Click { x: 10.0, y: 20.5 },
            ToRenderer::Select {
                x0: 1.0,
                y0: 2.0,
                x1: 30.5,
                y1: 40.5,
            },
            ToRenderer::DomDump,
            ToRenderer::Shutdown,
        ];
        for msg in msgs {
            assert_eq!(ToRenderer::decode(&msg.encode()).unwrap(), msg);
        }
    }

    #[test]
    fn to_browser_round_trip() {
        let msgs = [
            ToBrowser::Pong(7),
            ToBrowser::LoadStarted,
            ToBrowser::Title("hello".into()),
            ToBrowser::Frame {
                width: 2,
                height: 2,
                pixels: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            },
            ToBrowser::LoadDone,
            ToBrowser::Log("log line".into()),
            ToBrowser::NavigationRequest("https://example.com".into()),
            ToBrowser::DomDump("<html>".into()),
            ToBrowser::SelText("selected words".into()),
        ];
        for msg in msgs {
            assert_eq!(ToBrowser::decode(&msg.encode()).unwrap(), msg);
        }
    }

    #[test]
    fn frame_io_round_trip() {
        let payload = ToBrowser::Pong(123).encode();
        let mut buf = Vec::new();
        write_frame(&mut buf, &payload).unwrap();
        write_frame(&mut buf, &payload).unwrap();
        let mut cur = Cursor::new(&buf);
        assert_eq!(read_frame(&mut cur).unwrap(), payload);
        assert_eq!(read_frame(&mut cur).unwrap(), payload);
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(ToRenderer::decode(&[0xff]).is_err());
        assert!(ToBrowser::decode(&[TAG_FRAME, 1]).is_err());
    }
}
