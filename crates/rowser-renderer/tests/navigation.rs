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
        Self::with_js(false)
    }

    fn with_js(enabled: bool) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rowser-renderer"))
            .env("ROWSER_JS", if enabled { "1" } else { "0" })
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
        self.load_logs(message, title);
    }

    fn load_logs(&mut self, message: ToRenderer, title: &mut String) -> Vec<String> {
        let mut logs = Vec::new();
        let input = self.child.stdin.as_mut().unwrap();
        write_frame(input, &message.encode()).unwrap();
        input.flush().unwrap();
        loop {
            match self.events.recv_timeout(Duration::from_secs(30)).unwrap() {
                ToBrowser::LoadStarted => *title = "Loading…".into(),
                ToBrowser::Title(t) => *title = t,
                ToBrowser::Log(log) => logs.push(log),
                ToBrowser::LoadDone => break,
                _ => {}
            }
        }
        logs
    }
}

#[test]
fn navigate_and_reload_expose_document_fragment_to_scripts() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().take(6) {
            let mut stream = stream.unwrap();
            let mut request = [0; 4096];
            let n = stream.read(&mut request).unwrap();
            if String::from_utf8_lossy(&request[..n]).starts_with("GET /redirect ") {
                write!(stream, "HTTP/1.1 302 Found\r\nLocation: /target#redirect\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            } else {
                let body = "<h1>Hash route</h1><script>console.log('hash=' + location.hash)</script>";
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        }
    });
    let mut renderer = Renderer::with_js(true);
    let mut title = String::new();
    for (path, hash) in [("fixture", "#part"), ("redirect", "#redirect")] {
        let url = format!("{base}/{path}#part");
        for message in [ToRenderer::Navigate(url.clone()), ToRenderer::Reload(url)] {
            let logs = renderer.load_logs(message, &mut title);
            assert!(logs.iter().any(|log| log == &format!("[js console] hash={hash}")), "{path}: {logs:?}");
            let document_path = if path == "redirect" { "target" } else { path };
            assert_eq!(title, format!("{base}/{document_path}{hash}"));
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
    renderer.navigate(format!("{url}#other"), &mut title);
    assert_eq!(requests.load(Ordering::SeqCst), 2, "anchor after reload must not fetch");
    assert_eq!(title, "Revision 2");
    renderer.load(ToRenderer::Reload(url), &mut title);
    assert_eq!(requests.load(Ordering::SeqCst), 3, "hashless reload must refetch");
    assert_eq!(title, "Revision 3");
}

#[test]
fn reload_after_file_anchor_refetches_changed_document() {
    let path = std::env::temp_dir().join(format!("rowser-reload-{}.html", std::process::id()));
    std::fs::write(&path, "<title>Revision 1</title><h1 id='part'>Part</h1>").unwrap();
    let url = format!("file://{}", path.display());
    let mut renderer = Renderer::new();
    let mut title = String::new();
    renderer.navigate(url.clone(), &mut title);
    renderer.navigate(format!("{url}#part"), &mut title);
    assert_eq!(title, "Revision 1");
    std::fs::write(&path, "<title>Revision 2</title><h1 id='part'>Part</h1>").unwrap();
    renderer.load(ToRenderer::Reload(format!("{url}#part")), &mut title);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(title, "Revision 2", "file reload must refetch without the fragment");
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
