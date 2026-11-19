use rowser_ipc::{read_frame, write_frame, ToBrowser, ToRenderer};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
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

    fn navigate(&mut self, url: String, title: &mut String) -> Option<String> {
        self.load_events(ToRenderer::Navigate(url), title).1
    }

    fn load(&mut self, message: ToRenderer, title: &mut String) {
        self.load_events(message, title);
    }

    fn load_logs(&mut self, message: ToRenderer, title: &mut String) -> Vec<String> {
        self.load_events(message, title).0
    }

    fn load_events(&mut self, message: ToRenderer, title: &mut String) -> (Vec<String>, Option<String>) {
        let mut logs = Vec::new();
        let mut committed = None;
        let input = self.child.stdin.as_mut().unwrap();
        write_frame(input, &message.encode()).unwrap();
        input.flush().unwrap();
        loop {
            match self.events.recv_timeout(Duration::from_secs(30)).unwrap() {
                ToBrowser::LoadStarted => *title = "Loading…".into(),
                ToBrowser::Title(t) => *title = t,
                ToBrowser::Log(log) => logs.push(log),
                ToBrowser::NavigationCommitted(url) => committed = Some(url),
                ToBrowser::LoadDone => break,
                _ => {}
            }
        }
        (logs, committed)
    }

    /// Clicks at (x, y) and returns the URL the renderer asks to open, if
    /// any, plus the logs emitted meanwhile.
    fn click(&mut self, x: f32, y: f32) -> (Option<String>, Vec<String>) {
        let mut logs = Vec::new();
        let input = self.child.stdin.as_mut().unwrap();
        write_frame(input, &ToRenderer::Click { x, y }.encode()).unwrap();
        input.flush().unwrap();
        loop {
            match self.events.recv_timeout(Duration::from_secs(2)) {
                Ok(ToBrowser::NavigationRequest(url)) => return (Some(url), logs),
                Ok(ToBrowser::Log(log)) => logs.push(log),
                Ok(_) => {}
                Err(_) => return (None, logs),
            }
        }
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

fn fixture(name: &str) -> String {
    format!("file://{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn click_in_block_link_padding_navigates() {
    for js in [false, true] {
        let mut renderer = Renderer::with_js(js);
        renderer.navigate(fixture("links.html"), &mut String::new());
        assert_eq!(
            renderer.click(10.0, 10.0).0.as_deref(),
            Some("http://example.test/plain"),
            "js: {js}"
        );
    }
}

#[test]
fn prevented_link_click_does_not_navigate() {
    let mut renderer = Renderer::with_js(true);
    renderer.navigate(fixture("links.html"), &mut String::new());
    let (nav, logs) = renderer.click(10.0, 110.0);
    assert!(logs.iter().any(|l| l == "[js console] cancelled"), "listener ran: {logs:?}");
    assert_eq!(nav, None);
}

#[test]
fn link_click_cancelled_before_a_listener_throws_does_not_navigate() {
    let mut renderer = Renderer::with_js(true);
    renderer.navigate(fixture("links.html"), &mut String::new());
    let (nav, logs) = renderer.click(10.0, 610.0);
    assert_eq!(nav, None, "cancelled link navigated");
    assert!(
        logs.iter().any(|l| l.contains("Uncaught (in event listener)") && l.contains("lightbox failed")),
        "exception reported: {logs:?}"
    );
    assert!(logs.iter().any(|l| l == "[js console] after throw"), "later listener ran: {logs:?}");
}

#[test]
fn link_follows_href_set_by_click_handler() {
    let mut renderer = Renderer::with_js(true);
    renderer.navigate(fixture("links.html"), &mut String::new());
    assert_eq!(
        renderer.click(10.0, 210.0).0.as_deref(),
        Some("http://example.test/rewritten")
    );
    let (nav, logs) = renderer.click(45.0, 350.0);
    assert!(logs.iter().any(|l| l == "[js console] moved into elsewhere"), "listener ran: {logs:?}");
    assert_eq!(
        nav.as_deref(),
        Some("http://example.test/clicked"),
        "content moved out of the clicked link"
    );
    assert_eq!(
        renderer.click(10.0, 510.0).0.as_deref(),
        Some("http://example.test/late"),
        "href added by the handler"
    );
}

#[test]
fn navigation_reports_committed_redirect_url() {
    check_redirect_url("/final", "", "");
}

#[test]
fn navigation_preserves_redirect_fragments() {
    check_redirect_url("/final", "#details", "#details");
    check_redirect_url("/final#destination", "#details", "#destination");
    check_redirect_url("/final#", "#details", "#");
}

#[test]
fn fragment_navigation_after_redirect_does_not_refetch() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let server = std::thread::spawn(move || {
        let mut requests = 0;
        loop {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            if stream.read(&mut request).unwrap() == 0 {
                break;
            }
            requests += 1;
            let response = if requests == 1 {
                "HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
            } else {
                let body = "<title>Anchors</title><h1 id='a'>A</h1><div style='height: 1500px'></div><h1 id='b'>B</h1>";
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
            };
            stream.write_all(response.as_bytes()).unwrap();
        }
        requests
    });
    let mut renderer = Renderer::new();
    renderer.navigate(format!("{base}/start#a"), &mut String::new());
    write_frame(
        renderer.child.stdin.as_mut().unwrap(),
        &ToRenderer::Navigate(format!("{base}/final#b")).encode(),
    ).unwrap();
    let mut events = Vec::new();
    loop {
        let event = renderer.events.recv_timeout(Duration::from_secs(30)).unwrap();
        if event == ToBrowser::LoadDone {
            break;
        }
        events.push(event);
    }
    drop(TcpStream::connect(addr).unwrap());
    let requests = server.join().unwrap();
    assert_eq!(requests, 2, "fragment navigation refetched the document");
    assert!(!events.contains(&ToBrowser::LoadStarted));
    assert!(events.iter().all(|event| matches!(event,
        ToBrowser::Frame { .. } | ToBrowser::FrameRegion { .. } | ToBrowser::ScrollDelta { .. }
    )), "expected only scroll painting before LoadDone, got {events:?}");
}

fn check_redirect_url(location: &str, requested_fragment: &str, expected_fragment: &str) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let location = location.to_string();
    let server = std::thread::spawn(move || {
        for redirect in [true, false] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            stream.read(&mut request).unwrap();
            let response = if redirect {
                format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            } else {
                let body = "<title>Final document</title><h1>FINAL DESTINATION</h1>";
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
            };
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    let mut renderer = Renderer::new();
    let mut title = String::new();
    let committed = renderer.navigate(format!("{base}/start{requested_fragment}"), &mut title);
    server.join().unwrap();
    assert_eq!(committed, Some(format!("{base}/final{expected_fragment}")));
    assert_eq!(title, "Final document");
}

#[test]
fn navigation_reports_direct_and_error_document_urls() {
    let mut renderer = Renderer::new();
    let mut title = String::new();
    for url in ["about:blank", "file:///nonexistent-rowser-commit-test", ""] {
        assert_eq!(renderer.navigate(url.into(), &mut title), Some(url.into()));
    }
}
