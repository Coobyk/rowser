//! rowser-renderer: the per-tab child process.
//!
//! Speaks `rowser-ipc` on stdin/stdout. Loads documents (file://, built-in
//! rowser:// pages, error pages for everything else), runs them through the
//! engine pipeline — parse → layout → paint — and ships BGRA frames.

use rowser_ipc::{read_frame, write_frame, ToBrowser, ToRenderer};
use rowser_layout::{bgra, Layouter};
use rowser_text::FontCache;
use std::io::{self, Write};

const WELCOME: &str = include_str!("../../../assets/welcome.html");

/// Stage timing + event trace → shared log the browser owner can read.
fn dlog(msg: &str) {
    use std::io::Write as _;
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64 / 1000.0)
        .unwrap_or(0.0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/rowser-events.log")
    {
        let _ = writeln!(f, "{t:.3} [renderer] {msg}");
    }
}

macro_rules! pt {
    ($($a:tt)*) => {
        dlog(&format!($($a)*));
    };
}

/// Parallel subresource fetchers and their budgets.
const SUBRESOURCE_WORKERS: usize = 8;
const CSS_FETCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);
const IMG_FETCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);
const MAX_IMAGES_PER_LOAD: usize = 64;
/// Cap on the cross-navigation decoded-image cache (url → Image).
const MAX_IMG_CACHE: usize = 1024;
/// Per-socket read/write timeout — covers TLS handshake and response reads
/// (otherwise a stalled server hangs the fetch forever).
const FETCH_IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

fn main() {
    let stdout = io::stdout();
    let mut output = stdout.lock();

    // Reader thread pulls IPC frames off stdin into a channel so the main
    // loop can look ahead: back-to-back Scroll messages get coalesced into
    // a single repaint instead of queuing one full repaint per event.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        while let Ok(p) = read_frame(&mut input) {
            if tx.send(p).is_err() {
                return; // main loop exited
            }
        }
    });

    let mut fonts = FontCache::new();
    let mut page = Page::new();
    let mut pending: std::collections::VecDeque<ToRenderer> =
        std::collections::VecDeque::new();

    loop {
        let msg = if let Some(m) = pending.pop_front() {
            m
        } else {
            let Ok(payload) = rx.recv() else { break }; // parent hung up
            match ToRenderer::decode(&payload) {
                Ok(m) => m,
                Err(e) => {
                    let _ = send(&mut output, &ToBrowser::Log(format!("bad message: {e}")));
                    continue;
                }
            }
        };
        match msg {
            ToRenderer::Ping(n) => {
                let _ = send(&mut output, &ToBrowser::Pong(n));
            }
            ToRenderer::Navigate(url) => {
                let _ = send(&mut output, &ToBrowser::LoadStarted);
                let (base, frag) = url.split_once('#').map(|(b, f)| (b, f)).unwrap_or((url.as_str(), ""));
                let same_page = !page.cur_url.is_empty()
                    && (base == page.cur_url || (base.is_empty() && !frag.is_empty()));
                if same_page && !frag.is_empty() {
                    if let Some((_, y)) = page.anchors.iter().find(|(a, _)| a == frag) {
                        page.scroll = (*y - 8.0).clamp(0.0, page.max_scroll());
                        page.repaint_scrolled(&mut fonts);
                        let _ = send(&mut output, &frame_msg(&page));
                    }
                    let _ = send(&mut output, &ToBrowser::LoadDone);
                    continue;
                }
                let t0 = std::time::Instant::now();
                let (html, effective) = load_url(&url);
                let t_fetch = t0.elapsed();
                let t1 = std::time::Instant::now();
                let doc = rowser_html::parse(&html);
                pt!("[prof] navigate fetch={t_fetch:?} parse={:?}", t1.elapsed());
                // JS: run page scripts against the parsed DOM (SPAs build
                // their UI this way). Errors are logged, never fatal.
                let t_js = std::time::Instant::now();
                let (doc, js_logs) = if js_enabled() {
                    run_page_scripts(doc, &effective, page.viewport)
                } else {
                    (doc, Vec::new())
                };
                if !js_logs.is_empty() {
                    pt!("[prof] js exec {:?} ({} log lines)", t_js.elapsed(), js_logs.len());
                    for l in js_logs {
                        let _ = send(&mut output, &ToBrowser::Log(l));
                    }
                }
                let title = doc
                    .find_deep(doc.root, "title")
                    .map(|t| doc.text_content(t))
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| effective.clone());
                page.load(doc, effective, title.clone());
                page.repaint(&mut fonts);
                if !frag.is_empty() {
                    if let Some((_, y)) = page.anchors.iter().find(|(a, _)| a == frag) {
                        page.scroll = (*y - 8.0).clamp(0.0, page.max_scroll());
                        page.repaint_scrolled(&mut fonts);
                    }
                }
                let _ = send(&mut output, &ToBrowser::Title(title));
                let _ = send(&mut output, &frame_msg(&page));
                let _ = send(&mut output, &ToBrowser::LoadDone);
            }
            ToRenderer::ResizeViewport { width, height } => {
                page.viewport = (width.max(1), height.max(1));
                if page.clear_sel() {
                    let _ = send(&mut output, &ToBrowser::SelText(String::new()));
                }
                if page.has_doc() {
                    page.repaint(&mut fonts);
                    let _ = send(&mut output, &frame_msg(&page));
                } else {
                    let pixels = test_pattern(width.max(1), height.max(1));
                    let _ = send(
                        &mut output,
                        &ToBrowser::Frame {
                            width: width.max(1),
                            height: height.max(1),
                            pixels,
                        },
                    );
                }
            }
            ToRenderer::Click { x, y } => {
                // hit-test link regions in document space
                let mut found = None;
                for it in &page.items {
                    if let rowser_layout::DisplayItem::Text {
                        x: tx,
                        baseline,
                        text,
                        font,
                        size,
                        href: Some(href),
                        ..
                    } = it
                    {
                        let _ = (tx, font);
                        let w = fonts
                            .font(*font)
                            .map(|f| f.measure(text, *size))
                            .unwrap_or(0.0);
                        // rough hit box: from x to x+width, baseline-ascent to baseline+descent
                        if x >= *tx && x <= *tx + w && y >= baseline - size && y <= *baseline + size * 0.4 {
                            found = Some(href.clone());
                            break;
                        }
                    }
                }
                // A plain click collapses any selection — repaint to
                // erase the baked-in highlight (a link hit repaints via
                // the navigation it triggers instead).
                if let Some(url) = found {
                    page.clear_sel();
                    let _ = send(&mut output, &ToBrowser::NavigationRequest(url));
                } else if page.clear_sel() && page.has_doc() {
                    page.repaint_scrolled(&mut fonts);
                    let _ = send(&mut output, &frame_msg(&page));
                    let _ = send(&mut output, &ToBrowser::SelText(String::new()));
                }
            }
            ToRenderer::Select { x0, y0, x1, y1 } => {
                if !page.has_doc() {
                    continue;
                }
                // Coalesce back-to-back drags like Scroll: intermediate
                // frames are superseded, only the latest matters. The
                // last Select's full tuple is used — anchors are constant
                // within a drag, and this also survives two drags landing
                // in the queue back-to-back.
                let (mut ax, mut ay, mut fx, mut fy) = (x0, y0, x1, y1);
                while let Ok(p) = rx.try_recv() {
                    match ToRenderer::decode(&p) {
                        Ok(ToRenderer::Select { x0, y0, x1, y1 }) => {
                            ax = x0;
                            ay = y0;
                            fx = x1;
                            fy = y1;
                        }
                        Ok(m) => {
                            pending.push_back(m);
                            break;
                        }
                        Err(_) => {}
                    }
                }
                page.update_sel(&mut fonts, ax, ay, fx, fy);
                page.repaint_scrolled(&mut fonts);
                let _ = send(&mut output, &frame_msg(&page));
                let _ = send(&mut output, &ToBrowser::SelText(page.sel_text.clone()));
            }
            ToRenderer::Scroll { dy } => {
                // Coalesce any already-queued Scrolls into this repaint —
                // intermediate frames would be superseded anyway. Stops at
                // the first non-Scroll message (queued for next iteration).
                let mut total = dy;
                while let Ok(p) = rx.try_recv() {
                    match ToRenderer::decode(&p) {
                        Ok(ToRenderer::Scroll { dy: d }) => total += d,
                        Ok(m) => {
                            pending.push_back(m);
                            break;
                        }
                        Err(_) => {}
                    }
                }
                if page.has_doc() {
                    let new = (page.scroll + total).clamp(0.0, page.max_scroll());
                    if new != page.scroll {
                        page.scroll = new;
                        page.repaint_scrolled(&mut fonts);
                    }
                    // Unchanged offset: cached pixels are still valid — just
                    // resend the last frame instead of re-running the pipeline.
                    let _ = send(&mut output, &frame_msg(&page));
                }
            }
            ToRenderer::DomDump => {
                if let Some(doc) = &page.doc {
                    let _ = send(&mut output, &ToBrowser::DomDump(dom_dump(doc)));
                }
            }
            ToRenderer::Shutdown => break,
        }
    }
}

fn send(w: &mut impl Write, msg: &ToBrowser) -> io::Result<()> {
    write_frame(w, &msg.encode())
}

/// Background image pipeline messages. Fetch workers report raw bodies
/// (IO-bound — they run during layout without stealing CPU); decode jobs
/// run in a parallel pool at fill time and report decoded images.
enum ImgMsg {
    /// fetch result for a URL; None = fetch failed
    Body(Option<Vec<u8>>),
    /// decode result for a URL; None = decode failed
    Decoded(Option<rowser_image::Image>),
}

type ImgRx = std::sync::mpsc::Receiver<(String, ImgMsg)>;
type ImgTx = std::sync::mpsc::Sender<(String, ImgMsg)>;

struct Page {
    doc: Option<rowser_dom::Document>,
    viewport: (u32, u32),
    scroll: f32,
    content_h: f32,
    url_base: String,
    cur_url: String,
    anchors: Vec<(String, f32)>,
    pixels: Vec<u8>,
    items: Vec<rowser_layout::DisplayItem>,
    /// Scroll offset baked into `items` at the last layout. A scroll by dy
    /// is a pure -dy translation of every item unless `scroll_pinned`.
    items_scroll: f32,
    /// Any loaded/inline stylesheet uses `position:fixed|sticky` — those
    /// anchor to the viewport, so scrolling must re-run layout.
    scroll_pinned: bool,
    sheets: Vec<rowser_css::StyleSheet>,
    /// <img src> → decoded pixels for the current page.
    images: std::collections::HashMap<String, rowser_image::Image>,
    /// <img src>s that failed to fetch/decode — never retried.
    img_failed: std::collections::HashSet<String>,
    /// Resolved URL → fetch+decode result. Persists across navigations —
    /// a shared subresource (logo, stylesheet sprite) decodes once.
    img_by_url: std::collections::HashMap<String, Option<rowser_image::Image>>,
    /// Resolved URL → fetched body awaiting decode.
    img_bodies: std::collections::HashMap<String, Vec<u8>>,
    /// Resolved URLs a background worker is currently fetching or decoding.
    img_inflight: std::collections::HashSet<String>,
    /// Channel background fetch+decode workers report on. `img_tx` is kept
    /// so extra workers can be spawned onto the same stream at any time.
    img_tx: ImgTx,
    img_rx: ImgRx,
    /// First repaint after load waits for in-flight images; later repaints
    /// just drain whatever has arrived.
    img_waited: bool,
    /// Text-selection highlight rects (x, y, w, h) in the same viewport
    /// space as `items` — blended over `pixels` after every paint.
    sel: Vec<(f32, f32, f32, f32)>,
    /// Text covered by `sel`, for Ctrl+C. Kept in sync by `update_sel`.
    sel_text: String,
}

impl Page {
    fn new() -> Self {
        let (img_tx, img_rx) = std::sync::mpsc::channel();
        Self {
            doc: None,
            viewport: (800, 600),
            scroll: 0.0,
            content_h: 0.0,
            url_base: "about:blank".into(),
            cur_url: String::new(),
            anchors: Vec::new(),
            pixels: Vec::new(),
            items: Vec::new(),
            items_scroll: 0.0,
            scroll_pinned: false,
            sheets: Vec::new(),
            images: std::collections::HashMap::new(),
            img_failed: std::collections::HashSet::new(),
            img_by_url: std::collections::HashMap::new(),
            img_bodies: std::collections::HashMap::new(),
            img_inflight: std::collections::HashSet::new(),
            img_tx,
            img_rx,
            img_waited: false,
            sel: Vec::new(),
            sel_text: String::new(),
        }
    }

    fn has_doc(&self) -> bool {
        self.doc.is_some()
    }

    fn load(&mut self, doc: rowser_dom::Document, url: String, _title: String) {
        self.doc = Some(doc);
        self.url_base = url.clone();
        self.cur_url = url;
        self.scroll = 0.0;
        self.items_scroll = 0.0;
        self.sel.clear();
        self.sel_text.clear();
        self.images.clear();
        self.img_failed.clear();
        self.img_waited = false;
        self.sheets.clear();
        // Start image fetches immediately — they overlap the (blocking)
        // stylesheet fetch and layout, so first paint rarely waits.
        self.start_image_prefetch();
        self.load_linked_css();
        self.scroll_pinned = self.detect_scroll_pinned();
    }

    /// Fetch <link rel="stylesheet" href=...> documents into style sheets.
    fn load_linked_css(&mut self) {
        let Some(doc) = &self.doc else { return };
        let mut hrefs = Vec::new();
        for id in doc.descendants(doc.root) {
            if doc.tag(id) == Some("link") {
                let rel = doc.attr(id, "rel").unwrap_or("").to_lowercase();
                if rel.contains("stylesheet") {
                    if let Some(h) = doc.attr(id, "href") {
                        hrefs.push(h.to_string());
                    }
                }
            }
        }
        // Resolve + dedupe (document order preserved), then fetch in parallel
        // on a worker pool so each stylesheet's TLS handshake overlaps.
        let mut resolved: Vec<String> = Vec::new();
        for h in &hrefs {
            if let Some(u) = resolve_url(&self.url_base, h) {
                if !resolved.contains(&u) {
                    resolved.push(u);
                }
            }
        }
        let t = std::time::Instant::now();
        let bodies = fetch_pool(&resolved, CSS_FETCH_DEADLINE);
        pt!("[prof] css fetch {} urls in {:?}", resolved.len(), t.elapsed());
        for u in &resolved {
            if let Some(b) = bodies.get(u) {
                self.sheets
                    .push(rowser_css::parse(&String::from_utf8_lossy(b)));
            }
        }
    }

    /// Kick off background fetches of every <img src> in the document right
    /// after the HTML lands — transfers overlap the (blocking) stylesheet
    /// fetch and layout, so first paint rarely waits on the network.
    /// Decode is deferred to fill time: it is CPU-bound and would steal
    /// cycles from the single-threaded layout pass.
    fn start_image_prefetch(&mut self) {
        let Some(doc) = &self.doc else { return };
        let mut urls: Vec<String> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for id in doc.descendants(doc.root) {
            if doc.tag(id) != Some("img") {
                continue;
            }
            let Some(src) = doc.attr(id, "src") else { continue };
            if seen.iter().any(|s| s == src) {
                continue;
            }
            seen.push(src.to_string());
            match resolve_url(&self.url_base, src) {
                Some(u) => {
                    if self.img_by_url.contains_key(&u)
                        || self.img_inflight.contains(&u)
                        || urls.contains(&u)
                    {
                        continue;
                    }
                    urls.push(u);
                }
                // data:/unresolvable — permanently unavailable
                None => {
                    self.img_failed.insert(src.to_string());
                }
            }
            if urls.len() >= MAX_IMAGES_PER_LOAD {
                break;
            }
        }
        if !urls.is_empty() {
            self.spawn_img_workers(urls);
        }
    }

    /// Spawn a worker pool that fetches each resolved URL, reporting
    /// (url, Body) on the shared channel — failures report Body(None) so
    /// they are never retried by later repaints. Fetch is IO-bound, so
    /// these run concurrently with layout without stealing its CPU.
    fn spawn_img_workers(&mut self, urls: Vec<String>) {
        use std::collections::VecDeque;
        use std::sync::{Arc, Mutex};
        for u in &urls {
            self.img_inflight.insert(u.clone());
        }
        let queue = Arc::new(Mutex::new(urls.into_iter().collect::<VecDeque<_>>()));
        for _ in 0..SUBRESOURCE_WORKERS {
            let queue = Arc::clone(&queue);
            let tx = self.img_tx.clone();
            std::thread::spawn(move || loop {
                let url = queue.lock().unwrap().pop_front();
                let Some(url) = url else { break };
                let body = fetch_resolved(&url);
                if tx.send((url, ImgMsg::Body(body))).is_err() {
                    break;
                }
            });
        }
    }

/// Rasterize an SVG document body to an Image — `<img src="*.svg">` pieces
/// and icons. Rendered at 4× natural size for downscale antialiasing.
fn decode_svg(body: &[u8]) -> Option<rowser_image::Image> {
    let text = std::str::from_utf8(body).ok()?;
    if !text.trim_start().starts_with('<') || !text[..text.len().min(512)].contains("<svg") {
        return None;
    }
    let doc = rowser_html::parse(text);
    let svg = doc.find_deep(doc.root, "svg")?;
    let dim = |name: &str| -> Option<f32> {
        doc.attr(svg, name)
            .and_then(|v| v.trim_end_matches("px").parse().ok())
    };
    let (w, h) = match (dim("width"), dim("height")) {
        (Some(w), Some(h)) => (w, h),
        _ => doc
            .attr(svg, "viewBox")
            .or_else(|| doc.attr(svg, "viewbox"))
            .and_then(|v| {
                let n: Vec<f32> = v
                    .split(|c: char| c.is_whitespace() || c == ',')
                    .filter_map(|t| t.parse().ok())
                    .collect();
                (n.len() >= 4).then(|| (n[2], n[3]))
            })?,
    };
    let (rw, rh) = ((w * 4.0) as u32, (h * 4.0) as u32);
    Some(rowser_image::Image {
        width: rw,
        height: rh,
        pixels: rowser_layout::svg::render(&doc, svg, rw, rh, 0xff000000),
    })
}

    /// Spawn a worker pool decoding fetched bodies to Images — CPU-heavy,
    /// so it only runs at fill time when nothing else is on the main thread.
    fn spawn_decode_workers(&mut self, jobs: Vec<(String, Vec<u8>)>) {
        use std::collections::VecDeque;
        use std::sync::{Arc, Mutex};
        for (u, _) in &jobs {
            self.img_inflight.insert(u.clone());
        }
        let queue = Arc::new(Mutex::new(jobs.into_iter().collect::<VecDeque<_>>()));
        for _ in 0..SUBRESOURCE_WORKERS {
            let queue = Arc::clone(&queue);
            let tx = self.img_tx.clone();
            std::thread::spawn(move || loop {
                let job = queue.lock().unwrap().pop_front();
                let Some((url, body)) = job else { break };
                let img = rowser_image::decode(&body).ok().or_else(|| Self::decode_svg(&body));
                if tx.send((url, ImgMsg::Decoded(img))).is_err() {
                    break;
                }
            });
        }
    }

    /// Move finished background results into `img_bodies`/`img_by_url`.
    fn drain_images(&mut self) {
        while let Ok((u, msg)) = self.img_rx.try_recv() {
            self.img_inflight.remove(&u);
            match msg {
                ImgMsg::Body(Some(b)) => {
                    // Bodies for images layout never displays (display:none)
                    // are never consumed — bound the stash.
                    if self.img_bodies.len() < MAX_IMG_CACHE {
                        self.img_bodies.insert(u, b);
                    }
                }
                ImgMsg::Body(None) | ImgMsg::Decoded(None) => {
                    self.img_by_url.insert(u, None);
                }
                ImgMsg::Decoded(img) => {
                    if self.img_by_url.len() >= MAX_IMG_CACHE {
                        self.img_by_url.clear(); // bound session memory
                    }
                    self.img_by_url.insert(u, img);
                }
            }
        }
    }

    /// Unique display-list <img> srcs with no decoded image and no known
    /// failure, in display order.
    fn missing_srcs(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for it in &self.items {
            if let rowser_layout::DisplayItem::Image { src, .. } = it {
                if !self.images.contains_key(src)
                    && !self.img_failed.contains(src)
                    && !out.contains(src)
                {
                    out.push(src.clone());
                }
            }
        }
        out
    }

    /// Fill `images`/`img_failed` from `img_by_url` for every missing src,
    /// and kick decode jobs for bodies that have arrived. Returns resolved
    /// URLs still outstanding (fetch or decode in flight).
    fn sync_pending_images(&mut self) -> std::collections::HashSet<String> {
        let mut outstanding = std::collections::HashSet::new();
        let mut decode_jobs: Vec<(String, Vec<u8>)> = Vec::new();
        for src in self.missing_srcs() {
            match resolve_url(&self.url_base, &src) {
                None => {
                    self.img_failed.insert(src);
                }
                Some(u) => match self.img_by_url.get(&u) {
                    Some(Some(img)) => {
                        // Image isn't Clone — copy its public fields.
                        self.images.insert(
                            src,
                            rowser_image::Image {
                                width: img.width,
                                height: img.height,
                                pixels: img.pixels.clone(),
                            },
                        );
                    }
                    Some(None) => {
                        self.img_failed.insert(src);
                    }
                    None => {
                        if let Some(body) = self.img_bodies.remove(&u) {
                            decode_jobs.push((u.clone(), body));
                        }
                        outstanding.insert(u);
                    }
                },
            }
        }
        if !decode_jobs.is_empty() {
            self.spawn_decode_workers(decode_jobs);
        }
        outstanding
    }

    /// Block until every URL in `need` has a fetch result (`img_bodies` or
    /// `img_by_url`) or `deadline` expires. Decode jobs are kicked by the
    /// surrounding sync loop and report into `img_by_url` the same way.
    fn wait_images(&mut self, need: &std::collections::HashSet<String>, deadline: std::time::Duration) {
        let start = std::time::Instant::now();
        while need
            .iter()
            .any(|u| !self.img_by_url.contains_key(u) && !self.img_bodies.contains_key(u))
        {
            let left = deadline.saturating_sub(start.elapsed());
            if left.is_zero() {
                break;
            }
            match self.img_rx.recv_timeout(left) {
                Ok((u, msg)) => {
                    self.img_inflight.remove(&u);
                    match msg {
                        ImgMsg::Body(Some(b)) => {
                            if self.img_bodies.len() < MAX_IMG_CACHE {
                                self.img_bodies.insert(u, b);
                            }
                        }
                        ImgMsg::Body(None) | ImgMsg::Decoded(None) => {
                            self.img_by_url.insert(u, None);
                        }
                        ImgMsg::Decoded(img) => {
                            if self.img_by_url.len() >= MAX_IMG_CACHE {
                                self.img_by_url.clear();
                            }
                            self.img_by_url.insert(u, img);
                        }
                    }
                }
                Err(_) => break, // deadline hit
            }
        }
    }

    /// Make sure every image the display list references is decoded —
    /// or at least in flight. `wait` (first repaint of a load) blocks for
    /// stragglers up to IMG_FETCH_DEADLINE; later repaints just collect
    /// what finished so scrolling never stalls on the network.
    fn load_images(&mut self, wait: bool) {
        if !wait {
            self.drain_images();
            let outstanding = self.sync_pending_images();
            // Kick background fetches for urls nothing covers (e.g. img
            // tags introduced by a resize relayout); they fill on the next
            // repaint without stalling this one.
            let extra: Vec<String> = outstanding
                .iter()
                .filter(|u| {
                    !self.img_inflight.contains(*u) && !self.img_bodies.contains_key(*u)
                })
                .cloned()
                .collect();
            if !extra.is_empty() {
                self.spawn_img_workers(extra);
            }
            return;
        }
        let start = std::time::Instant::now();
        let mut extras_spawned = false;
        loop {
            self.drain_images();
            let outstanding = self.sync_pending_images();
            if outstanding.is_empty() {
                break;
            }
            if !extras_spawned {
                extras_spawned = true;
                // Resolved URLs no worker covers (e.g. beyond the prefetch
                // cap) get their own fetch pool on the same channel.
                let extra: Vec<String> = outstanding
                    .iter()
                    .filter(|u| {
                        !self.img_inflight.contains(*u) && !self.img_bodies.contains_key(*u)
                    })
                    .cloned()
                    .collect();
                if !extra.is_empty() {
                    self.spawn_img_workers(extra);
                }
            }
            let left = IMG_FETCH_DEADLINE.saturating_sub(start.elapsed());
            if left.is_zero() {
                break;
            }
            self.wait_images(&outstanding, left);
        }
    }

    /// `position:fixed`/`sticky` anchor to the viewport — a page using them
    /// can't reuse its display list across scroll offsets, so scrolling it
    /// re-runs layout. `absolute` (and everything else) shifts uniformly.
    fn detect_scroll_pinned(&self) -> bool {
        if std::env::var_os("ROWSER_FORCE_RELAYOUT").is_some() {
            return true; // testing hook: never take the translate fast path
        }
        if self.sheets.iter().any(sheet_pins_scroll) {
            return true;
        }
        let Some(doc) = &self.doc else { return false };
        if rowser_css::document_sheets(doc)
            .iter()
            .any(sheet_pins_scroll)
        {
            return true;
        }
        for id in doc.descendants(doc.root) {
            if let Some(s) = doc.attr(id, "style") {
                if style_attr_pins(s) {
                    return true;
                }
            }
        }
        false
    }

    fn max_scroll(&self) -> f32 {
        (self.content_h - self.viewport.1 as f32).max(0.0)
    }

    /// Repaint after a scroll. Pages without fixed/sticky positioning keep
    /// their display list — items just translate by the scroll delta, so a
    /// scroll costs one paint instead of a full re-layout.
    fn repaint_scrolled(&mut self, fonts: &mut FontCache) {
        if self.scroll_pinned {
            self.repaint(fonts);
            return;
        }
        let t0 = std::time::Instant::now();
        let dy = self.scroll - self.items_scroll;
        if dy != 0.0 {
            for it in &mut self.items {
                shift_item_y(it, -dy);
            }
            // Selection rects live in the same space — they follow the text.
            for r in &mut self.sel {
                r.1 -= dy;
            }
            self.items_scroll = self.scroll;
        }
        self.load_images(false);
        let t1 = std::time::Instant::now();
        self.pixels = rowser_paint::paint(
            &self.items,
            fonts,
            self.viewport.0,
            self.viewport.1,
            bgra(0xff, 0xff, 0xff),
            &self.images,
        );
        self.apply_sel();
        pt!(
            "[prof] scroll-repaint translate+images={:?} paint={:?}",
            t1 - t0,
            t1.elapsed()
        );
    }

    fn repaint(&mut self, fonts: &mut FontCache) {
        let Some(doc) = &self.doc else { return };
        let t0 = std::time::Instant::now();
        let (items, height, anchors) =
            Layouter::layout_vh(
                doc,
                fonts,
                self.viewport.0 as f32,
                self.scroll,
                self.viewport.1 as f32,
                &self.sheets,
            );
        let t_layout = t0.elapsed();
        self.content_h = height;
        self.items = items;
        self.items_scroll = self.scroll;
        self.anchors = anchors;
        let wait = !self.img_waited;
        self.img_waited = true;
        let t1 = std::time::Instant::now();
        self.load_images(wait);
        let t_img = t1.elapsed();
        let t2 = std::time::Instant::now();
        self.pixels = rowser_paint::paint(
            &self.items,
            fonts,
            self.viewport.0,
            self.viewport.1,
            bgra(0xff, 0xff, 0xff),
            &self.images,
        );
        self.apply_sel();
        pt!(
            "[prof] repaint layout={t_layout:?} images={t_img:?} paint={:?} items={} imgs={}",
            t2.elapsed(),
            self.items.len(),
            self.images.len()
        );
    }

    /// Recompute `sel`/`sel_text` for a drag from (x0,y0) to (x1,y1) —
    /// viewport-space device px, same coordinates `Click` uses.
    fn update_sel(&mut self, fonts: &mut FontCache, x0: f32, y0: f32, x1: f32, y1: f32) {
        let (rects, text) = compute_selection(&self.items, fonts, x0, y0, x1, y1);
        self.sel = rects;
        self.sel_text = text;
    }

    /// Drop the selection. Returns whether one existed (caller must
    /// repaint to erase the baked-in highlight).
    fn clear_sel(&mut self) -> bool {
        let had = !self.sel.is_empty() || !self.sel_text.is_empty();
        self.sel.clear();
        self.sel_text.clear();
        had
    }

    /// Blend the translucent selection tint over `pixels` at each `sel`
    /// rect. Runs after every paint so the highlight survives repaints;
    /// rects scroll with the items in `repaint_scrolled`.
    fn apply_sel(&mut self) {
        if self.sel.is_empty() {
            return;
        }
        const A: u32 = 110; // ~43% — readable through, clearly marked
        const R: u32 = 0x36;
        const G: u32 = 0x6c;
        const B: u32 = 0xd4;
        let (w, h) = (self.viewport.0 as i32, self.viewport.1 as i32);
        for &(x, y, sw, sh) in &self.sel {
            let x0 = (x as i32).clamp(0, w);
            let y0 = (y as i32).clamp(0, h);
            let x1 = ((x + sw).ceil() as i32).clamp(0, w);
            let y1 = ((y + sh).ceil() as i32).clamp(0, h);
            for yy in y0..y1 {
                let row = (yy * w + x0) as usize * 4;
                for i in (row..row + ((x1 - x0) as usize) * 4).step_by(4) {
                    let p = &mut self.pixels[i..i + 3];
                    p[0] = ((B * A + p[0] as u32 * (255 - A)) / 255) as u8;
                    p[1] = ((G * A + p[1] as u32 * (255 - A)) / 255) as u8;
                    p[2] = ((R * A + p[2] as u32 * (255 - A)) / 255) as u8;
                }
            }
        }
    }
}

/// One `DisplayItem::Text` (a laid-out word) flattened for hit-testing:
/// its line box plus the x offset of every char boundary.
struct TextBox<'a> {
    text: &'a str,
    x: f32,
    baseline: f32,
    top: f32,
    bot: f32,
    /// Char-boundary offsets relative to `x`; `starts.len() == chars + 1`.
    starts: Vec<f32>,
}

impl TextBox<'_> {
    fn nchars(&self) -> usize {
        self.starts.len() - 1
    }
}

/// Flatten the display list's Text items (already in reading order) into
/// hit-testable boxes. Char boundaries use the same kern+advance walk the
/// painter does, so clicks land where the ink actually is.
fn text_boxes<'a>(
    items: &'a [rowser_layout::DisplayItem],
    fonts: &mut FontCache,
) -> Vec<TextBox<'a>> {
    let mut out = Vec::new();
    for it in items {
        let rowser_layout::DisplayItem::Text {
            x,
            baseline,
            text,
            font,
            size,
            ..
        } = it
        else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        let (starts, top, bot) = match fonts.font(*font) {
            Some(f) => {
                let upem = f.units_per_em() as f32;
                let mut starts = Vec::with_capacity(text.chars().count() + 1);
                starts.push(0.0);
                let (mut pen, mut prev) = (0.0f32, 0u16);
                for c in text.chars() {
                    let gid = f.glyph_id(c);
                    pen += f.kern(prev, gid) as f32 * size / upem;
                    pen += f.advance(gid, *size);
                    starts.push(pen);
                    prev = gid;
                }
                let ascent = f.ascent(*size);
                (
                    starts,
                    baseline - ascent,
                    baseline - ascent + f.line_height(*size),
                )
            }
            // Font handle went missing — rough box so the item still selects.
            None => (
                (0..=text.chars().count())
                    .map(|i| i as f32 * size * 0.5)
                    .collect(),
                baseline - size,
                baseline + size * 0.4,
            ),
        };
        out.push(TextBox {
            text,
            x: *x,
            baseline: *baseline,
            top,
            bot,
            starts,
        });
    }
    out
}

/// Map a viewport-space point to a text position `(box, char boundary)`.
/// Snaps to the closest line when the point isn't inside any line box;
/// horizontally it lands on the nearest boundary of the closest word.
fn hit_pos(boxes: &[TextBox], x: f32, y: f32) -> Option<(usize, usize)> {
    // Best line band containing y: prefer the word whose x-range is
    // nearest, then the closest baseline (bands of adjacent lines can
    // overlap by a fraction of a px).
    let mut best: Option<(usize, f32, f32)> = None; // (idx, x-dist, baseline-dist)
    for (i, b) in boxes.iter().enumerate() {
        if y < b.top || y > b.bot {
            continue;
        }
        let xd = if x < b.x {
            b.x - x
        } else {
            (x - (b.x + b.starts[b.nchars()])).max(0.0)
        };
        let bd = (b.baseline - y).abs();
        if best.is_none_or(|(_, bx, bb)| xd < bx || (xd == bx && bd < bb)) {
            best = Some((i, xd, bd));
        }
    }
    let i = match best {
        Some((i, _, _)) => i,
        // No band contains y: snap to the closest line (nearest shared
        // baseline), then the x-nearest word on that line.
        None => {
            let mut by_line: Option<(usize, f32, f32)> = None;
            for (i, b) in boxes.iter().enumerate() {
                let xd = if x < b.x {
                    b.x - x
                } else {
                    (x - (b.x + b.starts[b.nchars()])).max(0.0)
                };
                let bd = (b.baseline - y).abs();
                if by_line.is_none_or(|(_, bb, bx)| bd < bb - 1.0 || (bd <= bb + 1.0 && xd < bx))
                {
                    by_line = Some((i, bd, xd));
                }
            }
            by_line?.0
        }
    };
    let b = &boxes[i];
    if x <= b.x {
        return Some((i, 0));
    }
    let rel = x - b.x;
    let mut k = b.nchars();
    for j in 0..b.nchars() {
        if (b.starts[j] - rel).abs() <= (b.starts[k] - rel).abs() {
            k = j;
        }
    }
    Some((i, k))
}

/// Char-level selection between two viewport-space points. Returns the
/// highlight rects (line-height tall, kern-accurate width) and the
/// covered text — words joined by ' ', line breaks by '\n'.
fn compute_selection(
    items: &[rowser_layout::DisplayItem],
    fonts: &mut FontCache,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
) -> (Vec<(f32, f32, f32, f32)>, String) {
    let boxes = text_boxes(items, fonts);
    let (Some(a), Some(b)) = (hit_pos(&boxes, x0, y0), hit_pos(&boxes, x1, y1)) else {
        return (Vec::new(), String::new());
    };
    let (a, b) = if a <= b { (a, b) } else { (b, a) };
    let mut rects = Vec::new();
    let mut text = String::new();
    let mut prev_baseline: Option<f32> = None;
    for (i, bx) in boxes.iter().enumerate().take(b.0 + 1).skip(a.0) {
        let s = if i == a.0 { a.1 } else { 0 };
        let e = if i == b.0 { b.1 } else { bx.nchars() };
        if s >= e {
            continue;
        }
        if !text.is_empty() {
            text.push(match prev_baseline {
                Some(pb) if (bx.baseline - pb).abs() < 1.0 => ' ',
                _ => '\n',
            });
        }
        prev_baseline = Some(bx.baseline);
        let sx = bx.x + bx.starts[s];
        let ex = bx.x + bx.starts[e];
        rects.push((sx.min(ex), bx.top, (ex - sx).abs(), bx.bot - bx.top));
        text.extend(bx.text.chars().skip(s).take(e - s));
    }
    (rects, text)
}

/// True if `sheet` declares `position:fixed|sticky` (or `position:var(...)`,
/// which could resolve to either — be conservative).
fn sheet_pins_scroll(sheet: &rowser_css::StyleSheet) -> bool {
    sheet.rules.iter().any(|r| {
        r.decls.iter().any(|d| {
            d.prop.trim().eq_ignore_ascii_case("position") && {
                let v = d.value.trim().to_ascii_lowercase();
                matches!(v.as_str(), "fixed" | "sticky") || v.contains("var(")
            }
        })
    })
}

/// Same check for an inline `style="..."` attribute.
fn style_attr_pins(style: &str) -> bool {
    style.split(';').any(|d| {
        let Some((k, v)) = d.split_once(':') else { return false };
        if !k.trim().eq_ignore_ascii_case("position") {
            return false;
        }
        let v = v.trim().to_ascii_lowercase();
        matches!(v.as_str(), "fixed" | "sticky") || v.contains("var(")
    })
}

/// Translate a display item vertically (scroll fast path).
fn shift_item_y(it: &mut rowser_layout::DisplayItem, dy: f32) {
    use rowser_layout::DisplayItem as D;
    match it {
        D::Text { baseline, .. } => *baseline += dy,
        D::FillRect { y, .. }
        | D::RoundedRect { y, .. }
        | D::Raster { y, .. }
        | D::Image { y, .. } => *y += dy,
    }
}

fn frame_msg(page: &Page) -> ToBrowser {
    ToBrowser::Frame {
        width: page.viewport.0,
        height: page.viewport.1,
        pixels: page.pixels.clone(),
    }
}

/// Resolve a URL to HTML text + the effective URL (after fallback).
fn load_url(url: &str) -> (String, String) {
    if let Some(rest) = url.strip_prefix("file://") {
        let path = rowser_net::percent_decode(rest).unwrap_or_else(|_| rest.to_string());
        return match std::fs::read_to_string(&path) {
            Ok(html) => (html, url.to_string()),
            Err(e) => (error_page(url, &format!("cannot read {path}: {e}")), url.to_string()),
        };
    }
    if url.starts_with("rowser://") {
        let name = url.trim_start_matches("rowser://").trim_end_matches('/');
        if name == "bookmarks" || name == "history" {
            return (list_page(name), url.to_string());
        }
        return (WELCOME.to_string(), url.to_string());
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return match web_fetch(url) {
            Ok(res) => (
                String::from_utf8_lossy(&res.body).into_owned(),
                res.url.to_string(),
            ),
            Err(e) => (error_page(url, &format!("network error: {e}")), url.to_string()),
        };
    }
    if url == "about:blank" {
        return ("<html><body></body></html>".into(), url.into());
    }
    // Bare path? try as file
    if std::path::Path::new(url).exists() {
        if let Ok(html) = std::fs::read_to_string(url) {
            return (html, format!("file://{url}"));
        }
    }
    (error_page(url, "unrecognized URL"), url.to_string())
}

/// `ROWSER_JS=0` kills page script execution entirely.
fn js_enabled() -> bool {
    std::env::var("ROWSER_JS").as_deref() != Ok("0")
}

/// Collect `<script>` elements in document order, fetch external bodies on
/// the shared fetch pool, then run everything against the Document on a
/// big-stack JS thread (the tree-walking interpreter burns real stack on
/// bundle-scale scripts). Returns the possibly-mutated Document plus log
/// lines for the browser console/Log IPC.
fn run_page_scripts(
    doc: rowser_dom::Document,
    page_url: &str,
    viewport: (u32, u32),
) -> (rowser_dom::Document, Vec<String>) {
    struct Script {
        src: Option<String>,
        body: String,
    }
    let mut scripts: Vec<Script> = Vec::new();
    for id in doc.descendants(doc.root) {
        if doc.tag(id) != Some("script") {
            continue;
        }
        // only executable types — skip json-ld, importmap, templates, etc.
        let ty = doc.attr(id, "type").unwrap_or("").trim().to_ascii_lowercase();
        let js_type = matches!(
            ty.as_str(),
            "" | "text/javascript"
                | "application/javascript"
                | "module"
                | "text/ecmascript"
                | "application/ecmascript"
        );
        if !js_type || doc.attr(id, "nomodule").is_some() {
            continue;
        }
        let src = doc.attr(id, "src").map(|s| s.to_string());
        let body = if src.is_none() {
            doc.children(id)
                .iter()
                .filter_map(|&c| doc.text(c))
                .collect::<String>()
        } else {
            String::new()
        };
        scripts.push(Script { src, body });
    }
    if scripts.is_empty() {
        return (doc, Vec::new());
    }
    // Resolve + fetch external bodies in parallel (same pool/deadline as CSS).
    let resolved: Vec<String> = scripts
        .iter()
        .filter_map(|s| s.src.as_ref())
        .filter_map(|s| resolve_url(page_url, s))
        .collect();
    let bodies = if resolved.is_empty() {
        std::collections::HashMap::new()
    } else {
        fetch_pool(&resolved, CSS_FETCH_DEADLINE)
    };
    // Script source per script (document order); fetch misses log + skip.
    let mut sources: Vec<Option<String>> = Vec::with_capacity(scripts.len());
    let mut pre_logs = Vec::new();
    for s in &scripts {
        match &s.src {
            Some(src) => match resolve_url(page_url, src).and_then(|u| bodies.get(&u)) {
                Some(b) => sources.push(Some(String::from_utf8_lossy(b).into_owned())),
                None => {
                    pre_logs.push(format!("[js] failed to fetch {src}"));
                    sources.push(None);
                }
            },
            None => sources.push(Some(s.body.clone())),
        }
    }
    // 32+ MiB stack: deep call chains in framework bundles would overflow
    // the default thread stack.
    let (vw, vh) = (viewport.0 as f64, viewport.1 as f64);
    let url = page_url.to_string();
    let handle = std::thread::Builder::new()
        .name("rowser-js".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let mut logs = pre_logs;
            let mut dc = rowser_js::dom_bridge::DomContext::with_viewport(doc, &url, vw, vh);
            // catch_unwind: a Rust panic inside the interpreter must not
            // take the Document down with it — return what we have.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                for src in sources.iter().flatten() {
                    if let Err(e) = dc.eval(src) {
                        logs.push(format!("[js] {e}"));
                    }
                }
                // React-style scheduling: rAF first, then one timers round
                // (which also drains microtasks + MessageChannel ports).
                dc.flush_raf();
                dc.run_timers();
            }));
            for e in &dc.errors {
                logs.push(format!("[js] {e}"));
            }
            for line in dc.ctx.take_output() {
                logs.push(format!("[js console] {line}"));
            }
            (dc.take_document(), logs)
        });
    match handle {
        Ok(h) => h.join().unwrap_or_else(|_| {
            // thread itself died (spawn-side panic before catch_unwind) —
            // we can't recover the Document; rebuild an empty-ish one.
            (rowser_html::parse("<html><body></body></html>"), vec![
                "[js] interpreter thread panicked — page rendered without JS DOM".to_string(),
            ])
        }),
        Err(e) => (
            // doc moved into the (failed) spawn — render a fallback
            rowser_html::parse("<html><body></body></html>"),
            vec![format!("[js] could not spawn interpreter thread: {e}")],
        ),
    }
}

fn error_page(url: &str, why: &str) -> String {
    format!(
        "<html><body><h1>Cannot display page</h1><p><b>{url}</b></p><p>{why}</p><hr><p><small>rowser error page</small></p></body></html>"
    )
}

/// Premultiplied-BGRA checkerboard for the blank/no-document state.
fn test_pattern(width: u32, height: u32) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut px = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let v = if ((x / 32) + (y / 32)) % 2 == 0 { 40u8 } else { 48u8 };
            px[i] = v;
            px[i + 1] = v;
            px[i + 2] = v + 8;
            px[i + 3] = 255;
        }
    }
    px
}


fn dom_dump(doc: &rowser_dom::Document) -> String {
    let mut s = String::new();
    dump_node(doc, doc.root, 0, &mut s);
    s
}

fn dump_node(doc: &rowser_dom::Document, id: rowser_dom::NodeId, d: usize, out: &mut String) {
    let pad = "  ".repeat(d.min(24));
    match doc.kind(id) {
        rowser_dom::NodeKind::Document => {}
        rowser_dom::NodeKind::Comment(_) => {}
        rowser_dom::NodeKind::Text(t) => {
            let t = t.trim();
            if !t.is_empty() {
                out.push_str(&format!("{pad}\"{}\"\n", t.chars().take(60).collect::<String>()));
            }
        }
        rowser_dom::NodeKind::Element { tag, attrs } => {
            let a: String = attrs
                .iter()
                .map(|(k, v)| format!(" {k}=\"{}\"", v.chars().take(40).collect::<String>()))
                .collect();
            out.push_str(&format!("{pad}<{tag}{a}>\n"));
        }
    }
    for &c in doc.children(id) {
        dump_node(doc, c, d + 1, out);
    }
}

/// rowser://bookmarks and rowser://history — built from ~/.config/rowser files.
fn list_page(name: &str) -> String {
    let file = format!("{}/.config/rowser/{name}.txt", std::env::var("HOME").unwrap_or_default());
    let mut items = String::new();
    if let Ok(text) = std::fs::read_to_string(&file) {
        for line in text.lines().rev() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            items.push_str(&format!("<li><a href=\"{}\">{}</a></li>", line, line));
        }
    }
    if items.is_empty() {
        items = "<li><i>nothing yet</i></li>".into();
    }
    format!("<html><body><h1>rowser:{name}</h1><ul>{items}</ul></body></html>")
}


/// Fetch a fully-resolved URL — http(s), file://, or plain path.
fn fetch_resolved(url: &str) -> Option<Vec<u8>> {
    if let Some(path) = url.strip_prefix("file://") {
        let path = rowser_net::percent_decode(path).unwrap_or_else(|_| path.to_string());
        return std::fs::read(path).ok();
    }
    if url.starts_with("http") {
        return web_fetch(url).ok().map(|r| r.body);
    }
    std::fs::read(url).ok()
}

/// Fetch `urls` on a small worker pool, returning url → body for everything
/// that completes before `deadline`. Serial fetching is a poor fit for the
/// web: each https request costs a full TLS handshake, so a page with dozens
/// of subresources would otherwise take minutes. Workers are detached; the
/// per-socket timeouts in web_fetch bound how long stragglers live.
fn fetch_pool(
    urls: &[String],
    deadline: std::time::Duration,
) -> std::collections::HashMap<String, Vec<u8>> {
    use std::collections::VecDeque;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Instant;

    let queue = Arc::new(Mutex::new(
        urls.iter().cloned().collect::<VecDeque<_>>(),
    ));
    let (tx, rx) = mpsc::channel::<(String, Vec<u8>)>();
    for _ in 0..urls.len().min(SUBRESOURCE_WORKERS) {
        let queue = Arc::clone(&queue);
        let tx = tx.clone();
        std::thread::spawn(move || loop {
            let url = queue.lock().unwrap().pop_front();
            let Some(url) = url else { break };
            if let Some(body) = fetch_resolved(&url) {
                if tx.send((url, body)).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    let mut out = std::collections::HashMap::new();
    let start = Instant::now();
    while out.len() < urls.len() {
        let left = deadline.saturating_sub(start.elapsed());
        if left.is_zero() {
            break;
        }
        match rx.recv_timeout(left) {
            Ok((url, body)) => {
                out.insert(url, body);
            }
            Err(_) => break, // timeout expired or all workers finished
        }
    }
    out
}

/// Resolve `resource` against `base` (absolute URLs pass through).
fn resolve_url(base: &str, resource: &str) -> Option<String> {
    let r = resource.trim();
    if r.starts_with("data:") {
        return None; // no data: URLs yet
    }
    if r.contains("://") || r.starts_with("file:") {
        return Some(r.to_string());
    }
    let Ok(base_u) = rowser_net::Url::parse(base) else {
        // base was a local path → join directories
        if r.starts_with('/') {
            return Some(format!("file://{r}"));
        }
        let dir = std::path::Path::new(base)
            .parent()
            .unwrap_or(std::path::Path::new("/"));
        return Some(format!("file://{}", dir.join(r).display()));
    };
    let resolved = base_u.resolve(r).ok()?;
    Some(resolved.to_string())
}


/// http/https fetch with cross-scheme redirect handling.
fn web_fetch(url: &str) -> io::Result<rowser_net::Response> {
    use std::net::TcpStream;
    let mut parsed = rowser_net::Url::parse(url)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    for _ in 0..10 {
        let resp = match parsed.scheme.as_str() {
            "http" => {
                let mut tcp = TcpStream::connect((parsed.host.as_str(), parsed.port))?;
                tcp.set_read_timeout(Some(FETCH_IO_TIMEOUT))?;
                tcp.set_write_timeout(Some(FETCH_IO_TIMEOUT))?;
                rowser_net::request_over(&mut tcp, &parsed)?
            }
            "https" => {
                let tcp = TcpStream::connect((parsed.host.as_str(), parsed.port))?;
                // SO_RCVTIMEO/SO_SNDTIMEO live on the socket — they stay in
                // effect once `tcp` is moved into the TlsStream.
                tcp.set_read_timeout(Some(FETCH_IO_TIMEOUT))?;
                tcp.set_write_timeout(Some(FETCH_IO_TIMEOUT))?;
                let mut tls =
                    rowser_tls::TlsStream::connect(&parsed.host, parsed.port, tcp)?;
                if let Err(e) = tls.verify_certificate() {
                    return Ok(rowser_net::Response {
                        url: parsed.to_string(),
                        status: 0,
                        headers: vec![],
                        body: format!(
                            "<html><body><h1>Certificate error</h1><p>{e}</p></body></html>"
                        )
                        .into_bytes(),
                    });
                }
                rowser_net::request_over(&mut tls, &parsed)?
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("scheme {other}"),
                ))
            }
        };
        let is_redir = matches!(resp.status, 301 | 302 | 303 | 307 | 308);
        if !is_redir {
            return Ok(resp);
        }
        let loc = resp
            .header("location")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no Location"))?;
        parsed = parsed
            .resolve(loc.trim())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "too many redirects",
    ))
}
