use rowser_ipc::{read_frame, write_frame, ToBrowser, ToRenderer};
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

struct Renderer {
    child: Child,
    events: Receiver<ToBrowser>,
}

impl Renderer {
    fn new() -> Self {
        Self::with_js(false)
    }

    fn with_js(js: bool) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rowser-renderer"))
            .env("ROWSER_JS", if js { "1" } else { "0" })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = child.stdout.take().unwrap();
        let (tx, events) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(payload) = read_frame(&mut output) {
                if tx.send(ToBrowser::decode(&payload).unwrap()).is_err() {
                    break;
                }
            }
        });
        Self { child, events }
    }

    fn navigate(&mut self, url: String, title: &mut String) {
        let input = self.child.stdin.as_mut().unwrap();
        write_frame(input, &ToRenderer::Navigate(url).encode()).unwrap();
        input.flush().unwrap();
        loop {
            match self.events.recv_timeout(Duration::from_secs(30)).unwrap() {
                ToBrowser::LoadStarted => *title = "Loading…".into(),
                ToBrowser::Title(t) => *title = t,
                ToBrowser::LoadDone => break,
                _ => {}
            }
        }
    }

    /// Clicks at (x, y) and returns the URL the renderer asks to open, if any.
    fn click(&mut self, x: f32, y: f32) -> Option<String> {
        let input = self.child.stdin.as_mut().unwrap();
        write_frame(input, &ToRenderer::Click { x, y }.encode()).unwrap();
        input.flush().unwrap();
        loop {
            match self.events.recv_timeout(Duration::from_secs(2)) {
                Ok(ToBrowser::NavigationRequest(url)) => return Some(url),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn same_page_navigation_preserves_tab_title() {
    let mut renderer = Renderer::new();
    let url = format!("file://{}/tests/fixtures/anchor.html", env!("CARGO_MANIFEST_DIR"));
    let mut title = String::new();
    renderer.navigate(url.clone(), &mut title);
    assert_eq!(title, "Anchor navigation test");
    for fragment in ["details", "missing", "details"] {
        renderer.navigate(format!("{url}#{fragment}"), &mut title);
        assert_eq!(title, "Anchor navigation test", "fragment: {fragment:?}");
    }
}

fn fixture(name: &str) -> String {
    format!("file://{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn click_in_block_link_padding_navigates() {
    for js in [false, true] {
        let mut renderer = Renderer::with_js(js);
        renderer.navigate(fixture("links.html"), &mut String::new());
        assert_eq!(
            renderer.click(10.0, 10.0).as_deref(),
            Some("http://example.test/plain"),
            "js: {js}"
        );
    }
}

#[test]
fn prevented_link_click_does_not_navigate() {
    let mut renderer = Renderer::with_js(true);
    renderer.navigate(fixture("links.html"), &mut String::new());
    assert_eq!(renderer.click(10.0, 110.0), None);
}
