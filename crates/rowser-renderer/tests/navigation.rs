use rowser_ipc::{read_frame, write_frame, ToBrowser, ToRenderer};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::Duration;

struct Renderer {
    child: Child,
    events: Receiver<ToBrowser>,
}

impl Renderer {
    fn new() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rowser-renderer"))
            .env("ROWSER_JS", "0")
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
        self.load(ToRenderer::Navigate(url), title);
    }

    fn load(&mut self, message: ToRenderer, title: &mut String) {
        let input = self.child.stdin.as_mut().unwrap();
        write_frame(input, &message.encode()).unwrap();
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
}

#[test]
fn reload_after_anchor_refetches_changed_document() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/fixture", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(3) {
            let mut stream = stream.unwrap();
            let mut request = [0; 4096];
            stream.read(&mut request).unwrap();
            let revision = count.fetch_add(1, Ordering::SeqCst) + 1;
            let body = format!("<title>Revision {revision}</title><h1 id='part'>Revision {revision}</h1>");
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    let mut renderer = Renderer::new();
    let mut title = String::new();
    renderer.navigate(url.clone(), &mut title);
    assert_eq!(title, "Revision 1");
    let anchored = format!("{url}#part");
    renderer.navigate(anchored.clone(), &mut title);
    assert_eq!(requests.load(Ordering::SeqCst), 1, "anchor must not fetch");
    renderer.load(ToRenderer::Reload(anchored), &mut title);
    assert_eq!(requests.load(Ordering::SeqCst), 2, "reload must refetch");
    assert_eq!(title, "Revision 2");
    renderer.load(ToRenderer::Reload(url), &mut title);
    assert_eq!(requests.load(Ordering::SeqCst), 3, "hashless reload must refetch");
    assert_eq!(title, "Revision 3");
}

#[test]
fn reload_empty_url_completes_with_error_page() {
    let mut renderer = Renderer::new();
    let mut title = String::new();
    renderer.load(ToRenderer::Reload(String::new()), &mut title);
    assert_eq!(title, "Cannot display page");
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
