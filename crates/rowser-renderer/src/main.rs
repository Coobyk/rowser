//! rowser-renderer: the per-tab child process.
//!
//! Speaks `rowser-ipc` on stdin/stdout (the socketpair handed to it by the
//! browser process at spawn). M0: responds to Ping, repaints a deterministic
//! test pattern on Navigate/ResizeViewport. No engine yet — this process is
//! where html/css/layout/text/paint will live.

use rowser_ipc::{read_frame, write_frame, ToBrowser, ToRenderer};
use std::io::{self, Write};

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();

    let mut viewport = (800u32, 600u32);
    let mut url = String::from("about:blank");

    loop {
        let payload = match read_frame(&mut input) {
            Ok(p) => p,
            Err(_) => break, // parent hung up
        };
        let msg = match ToRenderer::decode(&payload) {
            Ok(m) => m,
            Err(e) => {
                let _ = send(&mut output, &ToBrowser::Log(format!("bad message: {e}")));
                continue;
            }
        };
        match msg {
            ToRenderer::Ping(n) => {
                let _ = send(&mut output, &ToBrowser::Pong(n));
            }
            ToRenderer::Navigate(u) => {
                url = u;
                let _ = send(&mut output, &ToBrowser::LoadStarted);
                let pixels = test_pattern(viewport.0, viewport.1, &url);
                let _ = send(
                    &mut output,
                    &ToBrowser::Frame {
                        width: viewport.0,
                        height: viewport.1,
                        pixels,
                    },
                );
                let _ = send(&mut output, &ToBrowser::Title(format!("{url} — rowser")));
                let _ = send(&mut output, &ToBrowser::LoadDone);
            }
            ToRenderer::ResizeViewport { width, height } => {
                viewport = (width.max(1), height.max(1));
                let pixels = test_pattern(viewport.0, viewport.1, &url);
                let _ = send(
                    &mut output,
                    &ToBrowser::Frame {
                        width: viewport.0,
                        height: viewport.1,
                        pixels,
                    },
                );
            }
            ToRenderer::Shutdown => break,
        }
    }
}

fn send(w: &mut impl Write, msg: &ToBrowser) -> io::Result<()> {
    write_frame(w, &msg.encode())
}

/// FNV-1a hash of the URL → base color for the pattern.
fn hash(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Premultiplied BGRA test pattern (alpha = 255 throughout, so premultiply is
/// a no-op). Checkerboard tinted by URL hash, plus channel-order bars along
/// the bottom so a swapped channel is immediately visible: red, green, blue,
/// black, white.
fn test_pattern(width: u32, height: u32, seed: &str) -> Vec<u8> {
    let h = hash(seed);
    let r0 = (32 + (h & 0x7f)) as u8;
    let g0 = (32 + ((h >> 8) & 0x7f)) as u8;
    let b0 = (32 + ((h >> 16) & 0x7f)) as u8;

    let (w, hgt) = (width as usize, height as usize);
    let mut px = vec![0u8; w * hgt * 4];
    let bar_h = 48usize.min(hgt / 6);

    for y in 0..hgt {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let (r, g, b) = if hgt - y <= bar_h {
                // Channel-order check strip: R | G | B | black | white
                let seg = x * 5 / w.max(1);
                match seg.min(4) {
                    0 => (255, 0, 0),
                    1 => (0, 255, 0),
                    2 => (0, 0, 255),
                    3 => (0, 0, 0),
                    _ => (255, 255, 255),
                }
            } else {
                // 32px checkerboard between two shades of the hashed color
                let check = ((x / 32) + (y / 32)) % 2 == 0;
                if check {
                    (r0, g0, b0)
                } else {
                    (r0 / 3 + 24, g0 / 3 + 24, b0 / 3 + 24)
                }
            };
            px[i] = b;
            px[i + 1] = g;
            px[i + 2] = r;
            px[i + 3] = 255;
        }
    }
    px
}
