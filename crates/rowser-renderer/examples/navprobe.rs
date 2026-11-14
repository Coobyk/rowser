//! Drive the real rowser-renderer binary over IPC: Navigate → print all
//! ToBrowser messages. `navprobe <url>`
use rowser_ipc::{read_frame, write_frame, ToBrowser, ToRenderer};
use std::io::Write;
use std::process::{Command, Stdio};

fn main() {
    let url = std::env::args().nth(1).expect("usage: navprobe <url>");
    let bin = std::env::var("ROWSER_RENDERER_BIN")
        .unwrap_or_else(|_| "target/release/rowser-renderer".into());
    let mut p = Command::new(&bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn renderer");
    let mut stdin = p.stdin.take().unwrap();
    let mut stdout = p.stdout.take().unwrap();

    write_frame(&mut stdin, &ToRenderer::ResizeViewport { width: 1280, height: 800 }.encode()).unwrap();
    write_frame(&mut stdin, &ToRenderer::Navigate(url).encode()).unwrap();
    stdin.flush().unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    let mut navs = 0u32;
    while std::time::Instant::now() < deadline && navs < 5 {
        match read_frame(&mut stdout) {
            Ok(payload) => match ToBrowser::decode(&payload) {
                Ok(m) => {
                    let tag = match &m {
                        ToBrowser::Frame { width, height, .. } => format!("Frame {width}x{height}"),
                        ToBrowser::FrameRegion { .. } => "FrameRegion".into(),
                        ToBrowser::ScrollDelta { .. } => "ScrollDelta".into(),
                        ToBrowser::Log(_) => continue, // quiet
                        other => format!("{other:?}"),
                    };
                    println!("{tag}");
                    // Follow navigations like the browser shell does.
                    if let ToBrowser::NavigationRequest(u) = m {
                        navs += 1;
                        write_frame(&mut stdin, &ToRenderer::Navigate(u).encode()).unwrap();
                        stdin.flush().unwrap();
                    }
                }
                Err(e) => {
                    println!("bad frame: {e}");
                    break;
                }
            },
            Err(e) => {
                println!("EOF/err: {e}");
                break;
            }
        }
    }
    let _ = p.kill();
}
