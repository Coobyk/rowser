//! rowser-renderer: the per-tab child process.
//!
//! Speaks `rowser-ipc` on stdin/stdout. Loads documents (file://, built-in
//! rowser:// pages, error pages for everything else), runs them through the
//! engine pipeline — parse → layout → paint — and ships BGRA frames.

use rowser_ipc::{read_frame, write_frame, ToBrowser, ToRenderer};
use rowser_layout::{bgra, Layouter};
use rowser_text::FontCache;
use std::io::{self, Write};

mod fontface;

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
    let mut pending: std::collections::VecDeque<ToRenderer> = std::collections::VecDeque::new();

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
                let (base, frag) = url
                    .split_once('#')
                    .map(|(b, f)| (b, f))
                    .unwrap_or((url.as_str(), ""));
                let same_page = !page.cur_url.is_empty()
                    && (base == page.cur_url || (base.is_empty() && !frag.is_empty()));
                if same_page && !frag.is_empty() {
                    // :target styles may change layout — relayout only
                    // when a sheet could match, then scroll to the
                    // (freshly laid-out) anchor
                    page.state.target_frag = Some(frag.to_string());
                    if page.pseudo_mask & P_TARGET != 0 {
                        page.layout_dirty = true;
                    }
                    page.repaint(&mut fonts);
                    if let Some((_, y)) = page.anchors.iter().find(|(a, _)| a == frag) {
                        let y = *y;
                        let new = (y - 8.0).clamp(0.0, page.max_scroll());
                        if new != page.scroll {
                            page.scroll = new;
                            page.repaint_scrolled(&mut fonts);
                            page.dirty_all();
                        }
                    }
                    send_frame(&mut output, &mut page);
                    let _ = send(&mut output, &ToBrowser::LoadDone);
                    continue;
                }
                let t0 = std::time::Instant::now();
                let (html, effective) = load_url(&url);
                let t_fetch = t0.elapsed();
                let t1 = std::time::Instant::now();
                let doc = if js_enabled() {
                    rowser_html::parse_scripting(&html)
                } else {
                    rowser_html::parse(&html)
                };
                pt!("[prof] navigate fetch={t_fetch:?} parse={:?}", t1.elapsed());
                // JS: run page scripts against the parsed DOM (SPAs build
                // their UI this way). Errors are logged, never fatal.
                let t_js = std::time::Instant::now();
                let (doc, js_logs, js) = if js_enabled() {
                    run_page_scripts(doc, &effective, page.viewport)
                } else {
                    (doc, Vec::new(), None)
                };
                if !js_logs.is_empty() {
                    pt!(
                        "[prof] js exec {:?} ({} log lines)",
                        t_js.elapsed(),
                        js_logs.len()
                    );
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
                page.js = js;
                load_web_fonts(&page, &mut fonts);
                page.state.target_frag = if frag.is_empty() {
                    None
                } else {
                    Some(frag.to_string())
                };
                page.repaint(&mut fonts);
                if !frag.is_empty() {
                    if let Some((_, y)) = page.anchors.iter().find(|(a, _)| a == frag) {
                        page.scroll = (*y - 8.0).clamp(0.0, page.max_scroll());
                        page.repaint_scrolled(&mut fonts);
                    }
                }
                let _ = send(&mut output, &ToBrowser::Title(title));
                send_frame(&mut output, &mut page);
                let _ = send(&mut output, &ToBrowser::LoadDone);
            }
            ToRenderer::ResizeViewport { width, height } => {
                page.viewport = (width.max(1), height.max(1));
                page.layout_dirty = true;
                page.need_full = true;
                if page.clear_sel() {
                    let _ = send(&mut output, &ToBrowser::SelText(String::new()));
                }
                if page.has_doc() {
                    page.repaint(&mut fonts);
                    send_frame(&mut output, &mut page);
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
            ToRenderer::MouseMove { x, y, down } => {
                if !page.has_doc() {
                    continue;
                }
                // coalesce queued moves — only the latest position matters
                let (mut mx, mut my, mut md) = (x, y, down);
                while let Ok(p) = rx.try_recv() {
                    match ToRenderer::decode(&p) {
                        Ok(ToRenderer::MouseMove { x, y, down }) => {
                            mx = x;
                            my = y;
                            md = down;
                        }
                        Ok(m) => {
                            pending.push_back(m);
                            break;
                        }
                        Err(_) => {}
                    }
                }
                let hit = page.hit_at(mx, my);
                // Which dynamic pseudos changed? A relayout only pays off
                // when a sheet could restyle them — otherwise the repaint
                // produces identical pixels and nothing gets sent.
                let mut need = 0u8;
                if page.state.hovered != hit {
                    page.state.hovered = hit;
                    need |= P_HOVER;
                }
                // button release anywhere ends :active
                if !md && page.state.active.is_some() {
                    page.state.active = None;
                    need |= P_ACTIVE;
                }
                if need & page.pseudo_mask != 0 {
                    page.layout_dirty = true;
                    page.repaint(&mut fonts);
                    send_frame(&mut output, &mut page);
                }
                // cursor: CSS `cursor` on the hovered element (or default)
                let cur = hit.map(|n| page.hover_cursor(n)).unwrap_or_default();
                if cur != page.last_cursor {
                    page.last_cursor = cur.clone();
                    let _ = send(&mut output, &ToBrowser::Cursor(cur));
                }
            }
            ToRenderer::MouseDown { x, y } => {
                if page.has_doc() {
                    let hit = page.hit_at(x, y);
                    if page.state.active != hit {
                        page.state.active = hit;
                        // repaint only when an :active rule could match
                        if page.pseudo_mask & P_ACTIVE != 0 {
                            page.layout_dirty = true;
                            page.repaint(&mut fonts);
                            send_frame(&mut output, &mut page);
                        }
                    }
                }
            }
            ToRenderer::Click { x, y } => {
                // hit-test link regions in document space — the transform
                // stack maps each item's coords to painted position, so
                // the click is inverse-mapped back into that space
                // release ends :active; focus moves to the nearest
                // focusable ancestor of the hit (or clears)
                if page.state.active.is_some() {
                    page.state.active = None;
                    if page.pseudo_mask & P_ACTIVE != 0 {
                        page.layout_dirty = true;
                    }
                }
                let focus = page.hit_at(x, y).and_then(|n| page.focus_target(n));
                if page.state.focused != focus {
                    page.set_focus(focus); // marks layout_dirty itself
                }
                if page.layout_dirty && page.has_doc() {
                    page.repaint(&mut fonts);
                    send_frame(&mut output, &mut page);
                }
                // hit-test link regions in document space
                let mut found = None;
                let mut xf: Vec<rowser_layout::xform::Mat> = Vec::new();
                for it in &page.items {
                    match it {
                        rowser_layout::DisplayItem::PushTransform { m } => {
                            xf.push(*m);
                            continue;
                        }
                        rowser_layout::DisplayItem::PopTransform => {
                            xf.pop();
                            continue;
                        }
                        _ => {}
                    }
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
                        let (cx, cy) = xf
                            .iter()
                            .fold(rowser_layout::xform::Mat::IDENTITY, |a, m| a.mul(m))
                            .inverse()
                            .map(|inv| inv.apply(x, y))
                            .unwrap_or((x, y));
                        // rough hit box: from x to x+width, baseline-ascent to baseline+descent
                        if cx >= *tx
                            && cx <= *tx + w
                            && cy >= baseline - size
                            && cy <= *baseline + size * 0.4
                        {
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
                } else {
                    // Dispatch a synthetic click into the live JS context:
                    // hit-test picks the deepest (smallest) element box,
                    // the event bubbles to delegated listeners (React
                    // attaches at the root container), handlers mutate the
                    // DOM, and we repaint the fresh snapshot.
                    let target = page
                        .hits
                        .iter()
                        .filter(|(hx, hy, hw, hh, _, m)| {
                            // inverse-map the click into the hit's
                            // pre-transform space — exact for rotated /
                            // skewed / scaled elements, not just a bbox
                            let Some(inv) = m.inverse() else { return false };
                            let (px, py) = inv.apply(x, y);
                            px >= *hx && px <= hx + hw && py >= *hy && py <= hy + hh
                        })
                        .min_by(|a, b| {
                            (a.2 * a.3)
                                .partial_cmp(&(b.2 * b.3))
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .map(|h| h.4);
                    let mut dispatched = false;
                    if let (Some(js), Some(nid)) = (&page.js, target) {
                        let (rtx, rrx) = std::sync::mpsc::channel();
                        if js
                            .send(JsJob::Event {
                                nid,
                                ty: "click".into(),
                                client: (x as f64, y as f64),
                                reply: rtx,
                            })
                            .is_ok()
                        {
                            match rrx.recv() {
                                Ok(rep) => {
                                    dispatched = true;
                                    page.doc = Some(rep.doc);
                                    // The fresh DOM may carry new <style>
                                    // rules — relearn what layout depends
                                    // on before reusing anything.
                                    page.layout_dirty = true;
                                    page.pseudo_mask = page.detect_pseudo_mask();
                                    page.scroll_pinned = page.detect_scroll_pinned();
                                    if let Some((n, c)) = page.edit {
                                        // JS may have rewritten the
                                        // value — clamp the caret
                                        let len =
                                            page.control_value(n).chars().count();
                                        page.edit = Some((n, c.min(len)));
                                    }
                                    for l in rep.logs {
                                        let _ = send(&mut output, &ToBrowser::Log(l));
                                    }
                                    page.clear_sel();
                                    page.start_image_prefetch();
                                    page.repaint(&mut fonts);
                                    send_frame(&mut output, &mut page);
                                    let _ = send(&mut output, &ToBrowser::SelText(String::new()));
                                }
                                Err(_) => {
                                    page.js = None; // interpreter thread died
                                }
                            }
                        }
                    }
                    if !dispatched && page.clear_sel() && page.has_doc() {
                        page.repaint_scrolled(&mut fonts);
                        send_frame(&mut output, &mut page);
                        let _ = send(&mut output, &ToBrowser::SelText(String::new()));
                    }
                    // Native form submission: a click on a submit
                    // control navigates even when no JS handled it.
                    if let Some(ctl) = target.and_then(|n| page.submitter(n)) {
                        if let Some(url) =
                            page.enclosing_form(ctl).and_then(|f| page.submit_url(f))
                        {
                            let _ = send(&mut output, &ToBrowser::NavigationRequest(url));
                        }
                    }
                }
            }
            ToRenderer::Key { key, text, shift } => {
                if !page.has_doc() {
                    continue;
                }
                match page.key(&key, &text, shift) {
                    KeyOut::Navigate(url) => {
                        let _ = send(&mut output, &ToBrowser::NavigationRequest(url));
                    }
                    KeyOut::Repaint => {
                        page.repaint(&mut fonts);
                        send_frame(&mut output, &mut page);
                    }
                    KeyOut::None => {}
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
                send_frame(&mut output, &mut page);
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
                    let old = page.scroll;
                    let new = (page.scroll + total).clamp(0.0, page.max_scroll());
                    if new != page.scroll {
                        page.scroll = new;
                        page.repaint_scrolled(&mut fonts);
                        let d = new - old;
                        // Integral scroll on an unpinned page is a pure
                        // pixel shift — ship just the exposed strip and
                        // let the chrome move its stored frame. Anything
                        // else pending (relayout, newly decoded images)
                        // forces a full frame instead.
                        let msg = if !page.scroll_pinned
                            && !page.need_full
                            && d.fract() == 0.0
                            && d.abs() < page.viewport.1 as f32
                            && page.dirty.is_none()
                        {
                            Some(page.scroll_delta(d))
                        } else {
                            page.dirty_all();
                            page.take_frame()
                        };
                        if let Some(m) = msg {
                            let _ = send(&mut output, &m);
                        }
                    }
                    // Unchanged offset: the chrome's stored frame is
                    // already correct — send nothing at all.
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

/// A job for the persistent JS interpreter thread: dispatch a DOM event
/// at `nid` and reply with the post-event document snapshot + log lines.
enum JsJob {
    Event {
        nid: rowser_dom::NodeId,
        ty: String,
        client: (f64, f64),
        reply: std::sync::mpsc::Sender<JsReply>,
    },
}

struct JsReply {
    doc: rowser_dom::Document,
    logs: Vec<String>,
}

/// Outcome of a `ToRenderer::Key` press.
enum KeyOut {
    None,
    Repaint,
    Navigate(String),
}

/// `application/x-www-form-urlencoded` byte encoding — alphanumerics
/// and `*-._` pass through, space is `+`, everything else %XX.
fn form_urlencode(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Selected <option>'s value — `value` attr, else its text. First
/// option wins when none carry `selected` (the HTML default).
fn select_value(doc: &rowser_dom::Document, select: rowser_dom::NodeId) -> String {
    let opts: Vec<rowser_dom::NodeId> = doc
        .descendants(select)
        .filter(|&n| doc.tag(n) == Some("option"))
        .collect();
    let chosen = opts
        .iter()
        .find(|&&n| doc.attr(n, "selected").is_some())
        .or(opts.first());
    match chosen {
        Some(&o) => doc.attr(o, "value").map(String::from).unwrap_or_else(|| {
            doc.text_content(o).trim().to_string()
        }),
        None => String::new(),
    }
}

struct Page {
    doc: Option<rowser_dom::Document>,
    /// Live page-JS job channel — the interpreter thread owns the real
    /// document; `doc` is a snapshot refreshed after each dispatched event.
    js: Option<std::sync::mpsc::Sender<JsJob>>,
    /// Element border boxes (x, y, w, h, node, transform) for click
    /// hit-testing — the rect is in pre-transform space; the matrix maps
    /// it to painted position (inverse-mapped against click points).
    hits: Vec<rowser_layout::Hit>,
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
    /// `::selection` background tint (r,g,b) resolved from the document's
    /// stylesheets — lazily computed on the first apply_sel.
    sel_tint: Option<(u32, u32, u32)>,
    /// Interaction state (`:hover`/`:focus`/`:active`/`:target`) — fed
    /// into every layout pass so dynamic pseudo-classes restyle.
    state: rowser_css::PseudoState,
    /// Focused editable control + caret char index into its `value`.
    /// Set/cleared alongside `state.focused`; `state.caret` mirrors the
    /// index at each repaint.
    edit: Option<(rowser_dom::NodeId, usize)>,
    /// Last cursor keyword reported to the browser — `Cursor` frames are
    /// only sent when the resolved value changes.
    last_cursor: String,
    /// Viewport-space damage (x, y, w, h device px) accumulated since the
    /// last frame message — `None` means the painted pixels are unchanged
    /// and no frame needs sending at all.
    dirty: Option<Rect>,
    /// The chrome's stored framebuffer is stale or the wrong size — the
    /// next frame must be a full `Frame` (set on load and resize).
    need_full: bool,
    /// The display list is stale — set by anything layout depends on:
    /// DOM mutation, stylesheet change, viewport resize, a pseudo-state
    /// change a loaded rule can match, font load. `repaint` reuses
    /// `items` while this is clear.
    layout_dirty: bool,
    /// The editing caret moved with no layout-affecting change — a
    /// repaint may relocate the caret FillRect in place instead.
    caret_moved: bool,
    /// Bitmask (P_*) of dynamic pseudo-classes the page's sheets could
    /// match — hover/active/target state changes skip layout entirely
    /// when their bit is off.
    pseudo_mask: u8,
    /// <img> srcs that resolved (decoded or failed) in the last
    /// `load_images` — their item rects join the damage set.
    img_new: Vec<String>,
}

impl Page {
    fn new() -> Self {
        let (img_tx, img_rx) = std::sync::mpsc::channel();
        Self {
            doc: None,
            js: None,
            hits: Vec::new(),
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
            sel_tint: None,
            state: rowser_css::PseudoState::default(),
            edit: None,
            last_cursor: String::new(),
            dirty: None,
            need_full: false,
            layout_dirty: false,
            caret_moved: false,
            pseudo_mask: P_FOCUS,
            img_new: Vec::new(),
        }
    }

    fn has_doc(&self) -> bool {
        self.doc.is_some()
    }

    fn load(&mut self, doc: rowser_dom::Document, url: String, _title: String) {
        self.doc = Some(doc);
        self.js = None; // dropping the channel ends the old JS thread
        self.hits.clear();
        self.url_base = url.clone();
        self.cur_url = url;
        self.scroll = 0.0;
        self.items_scroll = 0.0;
        self.sel.clear();
        self.sel_text.clear();
        self.sel_tint = None;
        self.state = rowser_css::PseudoState::default();
        self.edit = None;
        self.last_cursor.clear();
        self.images.clear();
        self.img_failed.clear();
        self.img_waited = false;
        self.img_new.clear();
        self.items.clear();
        self.dirty = None;
        self.need_full = true;
        self.layout_dirty = true;
        self.caret_moved = false;
        self.sheets.clear();
        // Start image fetches immediately — they overlap the (blocking)
        // stylesheet fetch and layout, so first paint rarely waits.
        self.start_image_prefetch();
        self.load_linked_css();
        self.scroll_pinned = self.detect_scroll_pinned();
        self.pseudo_mask = self.detect_pseudo_mask();
        // autofocus: first element in doc order carrying the attribute
        // (if editable) takes focus so it's typable without a click
        if let Some(doc) = &self.doc {
            let af = doc
                .descendants(doc.root)
                .find(|&n| doc.attr(n, "autofocus").is_some());
            if let Some(n) = af.filter(|&n| self.editable(n)) {
                self.set_focus(Some(n));
            }
        }
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
        pt!(
            "[prof] css fetch {} urls in {:?}",
            resolved.len(),
            t.elapsed()
        );
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for u in &resolved {
            if let Some(b) = bodies.get(u) {
                let sh = rowser_css::parse_vw(&String::from_utf8_lossy(b), self.viewport.0 as f32);
                expand_imports(
                    sh,
                    u,
                    self.viewport.0 as f32,
                    &mut self.sheets,
                    0,
                    &mut seen,
                );
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
            let Some(src) = doc.attr(id, "src") else {
                continue;
            };
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
            density: 4.0,
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
                let img = rowser_image::decode(&body)
                    .ok()
                    .or_else(|| Self::decode_svg(&body));
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
            let src = match it {
                rowser_layout::DisplayItem::Image { src, .. }
                | rowser_layout::DisplayItem::BgImage { src, .. } => src,
                _ => continue,
            };
            if !self.images.contains_key(src)
                && !self.img_failed.contains(src)
                && !out.contains(src)
            {
                out.push(src.clone());
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
                    self.img_failed.insert(src.clone());
                    self.img_new.push(src);
                }
                Some(u) => match self.img_by_url.get(&u) {
                    Some(Some(img)) => {
                        // Image isn't Clone — copy its public fields.
                        self.images.insert(
                            src.clone(),
                            rowser_image::Image {
                                width: img.width,
                                height: img.height,
                                pixels: img.pixels.clone(),
                                density: img.density,
                            },
                        );
                        self.img_new.push(src);
                    }
                    Some(None) => {
                        self.img_failed.insert(src.clone());
                        self.img_new.push(src);
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
    fn wait_images(
        &mut self,
        need: &std::collections::HashSet<String>,
        deadline: std::time::Duration,
    ) {
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
        self.img_new.clear();
        if !wait {
            self.drain_images();
            let outstanding = self.sync_pending_images();
            // Kick background fetches for urls nothing covers (e.g. img
            // tags introduced by a resize relayout); they fill on the next
            // repaint without stalling this one.
            let extra: Vec<String> = outstanding
                .iter()
                .filter(|u| !self.img_inflight.contains(*u) && !self.img_bodies.contains_key(*u))
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

    /// Bitmask (P_*) of dynamic pseudo-classes the page's linked and
    /// inline sheets could match. `P_FOCUS` is always set — the UA
    /// stylesheet outlines focused controls and the edit caret needs
    /// layout regardless of author CSS.
    fn detect_pseudo_mask(&self) -> u8 {
        let mut mask = P_FOCUS;
        for sh in &self.sheets {
            mask |= sheet_mask(sh);
        }
        if let Some(doc) = &self.doc {
            for sh in rowser_css::document_sheets(doc).iter() {
                mask |= sheet_mask(sh);
            }
        }
        mask
    }

    fn max_scroll(&self) -> f32 {
        (self.content_h - self.viewport.1 as f32).max(0.0)
    }

    /// Deepest (smallest-area) element box containing the point — the same
    /// pick click dispatch uses, now shared by hover/active/focus.
    fn hit_at(&self, x: f32, y: f32) -> Option<rowser_dom::NodeId> {
        self.hits
            .iter()
            .filter(|(hx, hy, hw, hh, _, m)| {
                // inverse-map the point into the hit's pre-transform space
                let Some(inv) = m.inverse() else { return false };
                let (px, py) = inv.apply(x, y);
                px >= *hx && px <= hx + hw && py >= *hy && py <= hy + hh
            })
            .min_by(|a, b| {
                (a.2 * a.3)
                    .partial_cmp(&(b.2 * b.3))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|h| h.4)
    }

    /// Nearest focusable element at or above `node` — links, form
    /// controls, and anything with a `tabindex`.
    fn focus_target(&self, node: rowser_dom::NodeId) -> Option<rowser_dom::NodeId> {
        let doc = self.doc.as_ref()?;
        let mut cur = Some(node);
        while let Some(n) = cur {
            let focusable = matches!(
                doc.tag(n),
                Some("a") | Some("button") | Some("input") | Some("select") | Some("textarea")
            ) || doc.attr(n, "tabindex").is_some();
            if focusable {
                return Some(n);
            }
            cur = doc.parent(n);
        }
        None
    }

    /// Move focus to `focus` and (re)arm text editing when it lands on
    /// an editable control — caret starts at the end of the value.
    /// Any focus move re-lays out: the edit caret is a layout-drawn rect
    /// and the UA sheet gives controls a :focus outline.
    fn set_focus(&mut self, focus: Option<rowser_dom::NodeId>) {
        self.state.focused = focus;
        self.edit = focus
            .filter(|&n| self.editable(n))
            .map(|n| (n, self.control_value(n).chars().count()));
        self.layout_dirty = true;
    }

    /// `node` accepts typed text — text-like <input> types and
    /// <textarea>. Buttons/hidden/checkable controls are not editable.
    fn editable(&self, node: rowser_dom::NodeId) -> bool {
        let Some(doc) = &self.doc else { return false };
        match doc.tag(node) {
            Some("textarea") => true,
            Some("input") => !matches!(
                doc.attr(node, "type")
                    .map(|t| t.to_ascii_lowercase()),
                Some(t)
                    if matches!(
                        t.as_str(),
                        "hidden"
                            | "submit"
                            | "button"
                            | "reset"
                            | "image"
                            | "checkbox"
                            | "radio"
                            | "range"
                            | "color"
                            | "file"
                    )
            ),
            _ => false,
        }
    }

    /// An editable control's current text — its `value` attr (layout
    /// paints `value` for both <input> and <textarea>).
    fn control_value(&self, node: rowser_dom::NodeId) -> String {
        self.doc
            .as_ref()
            .and_then(|d| d.attr(node, "value"))
            .unwrap_or("")
            .to_string()
    }

    /// Ancestor <form> of `node`, if any.
    fn enclosing_form(&self, node: rowser_dom::NodeId) -> Option<rowser_dom::NodeId> {
        let doc = self.doc.as_ref()?;
        let mut cur = Some(node);
        while let Some(n) = cur {
            if doc.tag(n) == Some("form") {
                return Some(n);
            }
            cur = doc.parent(n);
        }
        None
    }

    /// Focusable elements in document order — the same set `focus_target`
    /// accepts: links, form controls, `tabindex`ed elements.
    fn focusables(&self) -> Vec<rowser_dom::NodeId> {
        let Some(doc) = &self.doc else { return Vec::new() };
        doc.descendants(doc.root)
            .filter(|&n| {
                matches!(
                    doc.tag(n),
                    Some("a") | Some("button") | Some("input") | Some("select") | Some("textarea")
                ) || doc.attr(n, "tabindex").is_some()
            })
            .collect()
    }

    /// Tab / Shift+Tab — cycle focus through `focusables`.
    fn tab_focus(&mut self, back: bool) {
        let list = self.focusables();
        if list.is_empty() {
            self.set_focus(None);
            return;
        }
        let idx = self
            .state
            .focused
            .and_then(|f| list.iter().position(|&n| n == f));
        let next = match (idx, back) {
            (Some(i), false) => (i + 1) % list.len(),
            (Some(i), true) => (i + list.len() - 1) % list.len(),
            (None, false) => 0,
            (None, true) => list.len() - 1,
        };
        self.set_focus(Some(list[next]));
    }

    /// `node` activates its form — <button> and <input> submit/image
    /// types (button defaults to type=submit). Climbs ancestors since
    /// buttons can nest phrasing content; stops at a <form> boundary.
    fn submitter(&self, node: rowser_dom::NodeId) -> Option<rowser_dom::NodeId> {
        let doc = self.doc.as_ref()?;
        let mut cur = Some(node);
        while let Some(n) = cur {
            match doc.tag(n) {
                Some("form") => return None,
                Some("button") => {
                    let t = doc.attr(n, "type").unwrap_or("submit").to_ascii_lowercase();
                    if t == "submit" || t.is_empty() {
                        return Some(n);
                    }
                    return None;
                }
                Some("input") => {
                    let t = doc.attr(n, "type").unwrap_or("text").to_ascii_lowercase();
                    if t == "submit" || t == "image" {
                        return Some(n);
                    }
                    return None;
                }
                _ => cur = doc.parent(n),
            }
        }
        None
    }

    /// Handle a key press aimed at the focused element. Returns what the
    /// caller should do about it.
    fn key(&mut self, key: &str, text: &str, shift: bool) -> KeyOut {
        if key == "tab" {
            self.tab_focus(shift);
            return KeyOut::Repaint;
        }
        if let Some((nid, mut cur)) = self.edit {
            // editing a text control — printable text inserts, editing
            // keys move/delete, enter submits the enclosing form
            let mut v: Vec<char> = self.control_value(nid).chars().collect();
            cur = cur.min(v.len());
            match key {
                "enter" => {
                    if let Some(url) = self
                        .enclosing_form(nid)
                        .and_then(|f| self.submit_url(f))
                    {
                        return KeyOut::Navigate(url);
                    }
                }
                "escape" => self.set_focus(None),
                "backspace" => {
                    if cur > 0 {
                        v.remove(cur - 1);
                        cur -= 1;
                    }
                }
                "delete" => {
                    if cur < v.len() {
                        v.remove(cur);
                    }
                }
                "left" => cur = cur.saturating_sub(1),
                "right" => cur = (cur + 1).min(v.len()),
                "home" => cur = 0,
                "end" => cur = v.len(),
                _ => {
                    if !text.is_empty() {
                        for c in text.chars() {
                            v.insert(cur, c);
                            cur += 1;
                        }
                    }
                }
            }
            let s: String = v.iter().collect();
            let caret0 = self.edit.map(|e| e.1);
            if s != self.control_value(nid) {
                if let Some(doc) = &mut self.doc {
                    doc.set_attr(nid, "value", &s);
                }
                // the value's text width changed — real layout geometry
                self.layout_dirty = true;
            }
            let still_editing = self.edit.is_some();
            self.edit = self.edit.map(|(n, _)| (n, cur));
            if !self.layout_dirty && still_editing && caret0 != Some(cur) {
                // caret-only move — repaint tries patching the caret rect
                self.caret_moved = true;
            }
            return if self.layout_dirty || self.caret_moved {
                KeyOut::Repaint
            } else {
                KeyOut::None
            };
        }
        // not editing: enter activates a focused link or submit control
        if key == "enter" {
            if let Some(f) = self.state.focused {
                if let Some(doc) = &self.doc {
                    if doc.tag(f) == Some("a") {
                        if let Some(h) = doc.attr(f, "href") {
                            if let Ok(u) =
                                rowser_net::Url::parse(&self.cur_url).and_then(|b| b.resolve(h))
                            {
                                return KeyOut::Navigate(u.to_string());
                            }
                        }
                    }
                }
                if let Some(ctl) = self.submitter(f) {
                    if let Some(url) =
                        self.enclosing_form(ctl).and_then(|f| self.submit_url(f))
                    {
                        return KeyOut::Navigate(url);
                    }
                }
            }
        }
        KeyOut::None
    }

    /// Build the submission URL for `form`: `action` resolved against
    /// the page URL with the successful controls serialized as a
    /// `application/x-www-form-urlencoded` query. POST forms submit as
    /// GET — the network layer is GET-only for now.
    fn submit_url(&self, form: rowser_dom::NodeId) -> Option<String> {
        let doc = self.doc.as_ref()?;
        let action = doc.attr(form, "action").unwrap_or(&self.cur_url);
        let base = rowser_net::Url::parse(&self.cur_url).ok()?;
        let mut url = base.resolve(action).ok()?;
        let mut data = String::new();
        for n in doc.descendants(form) {
            let Some(name) = doc.attr(n, "name") else { continue };
            if name.is_empty() {
                continue;
            }
            let val = match doc.tag(n) {
                Some("input") => {
                    let t = doc.attr(n, "type").unwrap_or("text").to_ascii_lowercase();
                    match t.as_str() {
                        "submit" | "button" | "reset" | "file" | "image" => continue,
                        "checkbox" | "radio" => {
                            if doc.attr(n, "checked").is_none() {
                                continue;
                            }
                            doc.attr(n, "value").unwrap_or("on").to_string()
                        }
                        _ => doc.attr(n, "value").unwrap_or("").to_string(),
                    }
                }
                Some("textarea") => match doc.attr(n, "value") {
                    Some(v) => v.to_string(),
                    None => doc.text_content(n),
                },
                Some("select") => select_value(doc, n),
                _ => continue,
            };
            if !data.is_empty() {
                data.push('&');
            }
            data.push_str(&form_urlencode(name));
            data.push('=');
            data.push_str(&form_urlencode(&val));
        }
        url.query = Some(data);
        url.fragment = None;
        if doc.attr(form, "method").map(|m| m.eq_ignore_ascii_case("post")) == Some(true) {
            dlog("form method=post submitted as GET (no POST support yet)");
        }
        Some(url.to_string())
    }

    /// CSS `cursor` keyword for `node` under the current pseudo-state.
    fn hover_cursor(&self, node: rowser_dom::NodeId) -> String {
        let Some(doc) = &self.doc else {
            return String::new();
        };
        rowser_layout::cursor_for(doc, self.viewport.0 as f32, &self.sheets, node, &self.state)
    }

    /// Union `r` into the pending viewport-space damage.
    fn dirty_rect(&mut self, r: Rect) {
        self.dirty = Some(match self.dirty {
            Some(d) => rect_union(d, r),
            None => r,
        });
    }

    /// Damage the whole viewport — every pixel may differ.
    fn dirty_all(&mut self) {
        self.dirty = Some((0.0, 0.0, self.viewport.0 as f32, self.viewport.1 as f32));
    }

    /// Union the current selection highlight rects into the damage — call
    /// before mutating `sel` (erase) and after (draw).
    fn dirty_sel(&mut self) {
        let mut acc: Option<Rect> = None;
        for &r in &self.sel {
            acc = Some(acc.map_or(r, |a| rect_union(a, r)));
        }
        if let Some(r) = acc {
            self.dirty_rect(r);
        }
    }

    /// Union the rects of items whose <img> src just resolved — their
    /// display items didn't change, but their paint did.
    fn dirty_new_images(&mut self) {
        if self.img_new.is_empty() {
            return;
        }
        let new = std::mem::take(&mut self.img_new);
        let mut acc: Option<Rect> = None;
        for it in &self.items {
            let (x, y, w, h, src) = match it {
                rowser_layout::DisplayItem::Image { x, y, w, h, src, .. }
                | rowser_layout::DisplayItem::BgImage { x, y, w, h, src, .. } => {
                    (*x, *y, *w, *h, src)
                }
                _ => continue,
            };
            if new.iter().any(|s| s == src) {
                let r = (x, y, w, h);
                acc = Some(acc.map_or(r, |a| rect_union(a, r)));
            }
        }
        if let Some(r) = acc {
            self.dirty_rect(r);
        }
    }

    /// Relocate the editing caret's FillRect in place — same geometry,
    /// same item count, just a new x — so a caret move costs a paint,
    /// not a layout. Returns false when the caret can't be identified
    /// unambiguously; the caller re-lays out instead.
    fn patch_caret(&mut self, fonts: &mut FontCache) -> bool {
        let Some((nid, ci)) = self.edit else {
            return false;
        };
        // Painted bounds of the focused control: hit rects live in
        // pre-transform space — map the corners through like hit-testing
        // inverse-maps clicks.
        let mut bbox: Option<Rect> = None;
        for &(hx, hy, hw, hh, n, m) in &self.hits {
            if n != nid {
                continue;
            }
            let (mut x0, mut y0, mut x1, mut y1) =
                (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for (cx, cy) in [(hx, hy), (hx + hw, hy), (hx, hy + hh), (hx + hw, hy + hh)] {
                let (px, py) = m.apply(cx, cy);
                x0 = x0.min(px);
                y0 = y0.min(py);
                x1 = x1.max(px);
                y1 = y1.max(py);
            }
            let r = (x0, y0, x1 - x0, y1 - y0);
            bbox = Some(bbox.map_or(r, |b| rect_union(b, r)));
        }
        let Some((bx, by, bw, bh)) = bbox else {
            return false;
        };
        // The caret is the only narrow FillRect layout emits inside a text
        // control — zero or several candidates leave it ambiguous.
        let mut idx = None;
        for (i, it) in self.items.iter().enumerate() {
            let rowser_layout::DisplayItem::FillRect { x, y, w, h, .. } = it else {
                continue;
            };
            if *w < 0.5
                || *w > 8.0
                || *x + w < bx
                || *x > bx + bw
                || *y + h < by
                || *y > by + bh
            {
                continue;
            }
            if idx.is_some() {
                return false;
            }
            idx = Some(i);
        }
        let Some(i) = idx else { return false };
        // The sibling Text carries the control's value — the word path
        // emits the caret right before it, the clipped control path right
        // after. Measure the prefix up to the caret index for the new x.
        let j = [i + 1, i.wrapping_sub(1)]
            .into_iter()
            .find(|&j| {
                matches!(self.items.get(j), Some(rowser_layout::DisplayItem::Text { .. }))
            });
        let Some(j) = j else { return false };
        let Some(rowser_layout::DisplayItem::Text {
            x,
            text,
            font,
            size,
            ls,
            ..
        }) = self.items.get(j)
        else {
            return false;
        };
        let (tx, text, font, size, ls) = (*x, text.clone(), *font, *size, *ls);
        let prefix: String = text.chars().take(ci).collect();
        let cw = fonts
            .font(font)
            .map(|f| f.measure(&prefix, size))
            .unwrap_or(0.0)
            + ls * prefix.chars().count() as f32;
        let new_x = (tx + cw).clamp(bx, bx + bw - 1.0);
        let rowser_layout::DisplayItem::FillRect { x, y, w, h, .. } = &mut self.items[i]
        else {
            return false;
        };
        let old_r = (*x, *y, *w, *h);
        *x = new_x;
        let new_r = (*x, *y, *w, *h);
        self.dirty_rect(old_r);
        self.dirty_rect(new_r);
        true
    }

    /// Consume pending damage into the cheapest sufficient frame message
    /// for the chrome: `Frame` when its stored buffer needs a refresh,
    /// `FrameRegion` for a sub-rect — `None` when the pixels the chrome
    /// already holds are correct.
    fn take_frame(&mut self) -> Option<ToBrowser> {
        let (vw, vh) = self.viewport;
        if self.pixels.len() != vw as usize * vh as usize * 4 {
            // No correctly-sized bitmap to crop from — nothing valid to send.
            self.dirty = None;
            return None;
        }
        let whole = (0.0, 0.0, vw as f32, vh as f32);
        if self.need_full || self.dirty == Some(whole) {
            self.need_full = false;
            self.dirty = None;
            return Some(ToBrowser::Frame {
                width: vw,
                height: vh,
                pixels: self.pixels.clone(),
            });
        }
        let (dx, dy, dw, dh) = self.dirty.take()?;
        // Inflate a few px — glyph ink and shadows can spill past an
        // item's nominal box.
        const MARGIN: f32 = 4.0;
        let x = (dx - MARGIN).floor().max(0.0) as i32;
        let y = (dy - MARGIN).floor().max(0.0) as i32;
        let x2 = (dx + dw + MARGIN).ceil().min(vw as f32) as i32;
        let y2 = (dy + dh + MARGIN).ceil().min(vh as f32) as i32;
        let (w, h) = (x2 - x, y2 - y);
        if w <= 0 || h <= 0 {
            return None;
        }
        let stride = vw as usize * 4;
        let mut px = Vec::with_capacity(w as usize * h as usize * 4);
        for row in y..y + h {
            let s = row as usize * stride + x as usize * 4;
            px.extend_from_slice(&self.pixels[s..s + w as usize * 4]);
        }
        Some(ToBrowser::FrameRegion {
            x: x as u32,
            y: y as u32,
            width: w as u32,
            height: h as u32,
            pixels: px,
        })
    }

    /// Frame message for an integral scroll of an unpinned page: the
    /// stored frame shifts by `d` device px and only the newly exposed
    /// strip ships. Caller guarantees `d.fract() == 0` and `|d| < vh`.
    fn scroll_delta(&mut self, d: f32) -> ToBrowser {
        let (vw, vh) = (self.viewport.0 as i32, self.viewport.1 as i32);
        let d = d as i32;
        // d > 0 (scrolled down): content moved up, new strip at the bottom.
        let (y, h) = if d > 0 { (vh - d, d) } else { (0, -d) };
        let stride = vw as usize * 4;
        let mut px = Vec::with_capacity(vw as usize * h as usize * 4);
        for row in y..y + h {
            let s = row as usize * stride;
            px.extend_from_slice(&self.pixels[s..s + stride]);
        }
        self.dirty = None;
        ToBrowser::ScrollDelta {
            dy: d,
            x: 0,
            y: y as u32,
            width: vw as u32,
            height: h as u32,
            pixels: px,
        }
    }

    /// Repaint after a scroll. Pages without fixed/sticky positioning keep
    /// their display list — items just translate by the scroll delta, so a
    /// scroll costs one paint instead of a full re-layout.
    fn repaint_scrolled(&mut self, fonts: &mut FontCache) {
        // Pinned pages anchor to the viewport and a stale display list is
        // no list at all — both take the full layout path.
        if self.scroll_pinned || self.layout_dirty {
            self.layout_dirty = true;
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
            for h in &mut self.hits {
                h.1 -= dy;
                h.5 = h.5.conjugated(0.0, -dy);
            }
            self.items_scroll = self.scroll;
        }
        self.load_images(false);
        self.dirty_new_images();
        let t1 = std::time::Instant::now();
        self.pixels = rowser_paint::gpu::paint_auto(
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

    /// Produce fresh pixels honoring the cheapest sufficient path: a full
    /// re-layout only when `layout_dirty`, an in-place caret patch when
    /// only the caret moved, otherwise a plain repaint of `items`.
    fn repaint(&mut self, fonts: &mut FontCache) {
        self.state.caret = self.edit.map(|e| e.1);
        if self.doc.is_none() {
            return;
        }
        let t0 = std::time::Instant::now();
        let mut t_layout = std::time::Duration::ZERO;
        if self.layout_dirty {
            let old_items = std::mem::take(&mut self.items);
            let doc = self.doc.as_ref().unwrap();
            let (items, height, anchors, hits) = Layouter::layout_vh_st(
                doc,
                fonts,
                self.viewport.0 as f32,
                self.scroll,
                self.viewport.1 as f32,
                &self.sheets,
                &self.state,
            );
            t_layout = t0.elapsed();
            self.content_h = height;
            // Damage = union of differing items' painted bounds — identical
            // items repaint identically, so untouched regions stay clean.
            match diff_damage(&old_items, &items, fonts) {
                Damage::All => self.dirty_all(),
                Damage::Region(r) => self.dirty_rect(r),
                Damage::Clean => {}
            }
            self.items = items;
            self.hits = hits;
            self.items_scroll = self.scroll;
            self.anchors = anchors;
            self.layout_dirty = false;
            self.caret_moved = false;
        } else if self.caret_moved {
            if self.patch_caret(fonts) {
                self.caret_moved = false;
            } else {
                // Ambiguous caret — re-lay out after all.
                self.layout_dirty = true;
                self.repaint(fonts);
                return;
            }
        }
        let wait = !self.img_waited;
        self.img_waited = true;
        let t1 = std::time::Instant::now();
        self.load_images(wait);
        self.dirty_new_images();
        let t_img = t1.elapsed();
        let t2 = std::time::Instant::now();
        self.pixels = rowser_paint::gpu::paint_auto(
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
        self.dirty_sel();
        let (rects, text) = compute_selection(&self.items, fonts, x0, y0, x1, y1);
        self.sel = rects;
        self.sel_text = text;
        self.dirty_sel();
    }

    /// Drop the selection. Returns whether one existed (caller must
    /// repaint to erase the baked-in highlight).
    fn clear_sel(&mut self) -> bool {
        let had = !self.sel.is_empty() || !self.sel_text.is_empty();
        if had {
            self.dirty_sel();
        }
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
        // ::selection background-color (html/body ::selection rules) —
        // falls back to the default blue when no rule declares one
        if self.sel_tint.is_none() {
            if let Some(doc) = &self.doc {
                self.sel_tint = Some(
                    Layouter::selection_tint(doc, &self.sheets, self.viewport.0 as f32)
                        .map(|c| ((c >> 16) & 0xff, (c >> 8) & 0xff, c & 0xff))
                        .unwrap_or((0x36, 0x6c, 0xd4)),
                );
            }
        }
        const A: u32 = 110; // ~43% — readable through, clearly marked
        let (tr, tg, tb) = self.sel_tint.unwrap_or((0x36, 0x6c, 0xd4));
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
                    p[0] = ((tb * A + p[0] as u32 * (255 - A)) / 255) as u8;
                    p[1] = ((tg * A + p[1] as u32 * (255 - A)) / 255) as u8;
                    p[2] = ((tr * A + p[2] as u32 * (255 - A)) / 255) as u8;
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
            sel,
            ..
        } = it
        else {
            continue;
        };
        if text.is_empty() || !sel {
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
                if by_line.is_none_or(|(_, bb, bx)| bd < bb - 1.0 || (bd <= bb + 1.0 && xd < bx)) {
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
            let p = d.prop.trim().to_ascii_lowercase();
            let v = d.value.trim().to_ascii_lowercase();
            match p.as_str() {
                "position" => matches!(v.as_str(), "fixed" | "sticky") || v.contains("var("),
                // background-attachment:fixed pins the same way — the page
                // must relayout on scroll for the fixed positioning area
                "background" | "background-attachment" => v
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                    .any(|t| t == "fixed"),
                _ => false,
            }
        })
    })
}

/// Same check for an inline `style="..."` attribute.
fn style_attr_pins(style: &str) -> bool {
    style.split(';').any(|d| {
        let Some((k, v)) = d.split_once(':') else {
            return false;
        };
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim().to_ascii_lowercase();
        match k.as_str() {
            "position" => matches!(v.as_str(), "fixed" | "sticky") || v.contains("var("),
            "background" | "background-attachment" => v
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .any(|t| t == "fixed"),
            _ => false,
        }
    })
}

/// Translate a display item vertically (scroll fast path).
fn shift_item_y(it: &mut rowser_layout::DisplayItem, dy: f32) {
    use rowser_layout::DisplayItem as D;
    match it {
        D::Text { baseline, .. } => *baseline += dy,
        D::FillRect { y, .. }
        | D::RoundedRect { y, .. }
        | D::RoundedRect2 { y, .. }
        | D::BorderSide { y, .. }
        | D::Shadow { y, .. }
        | D::PushClip { y, .. }
        | D::Raster { y, .. }
        | D::Image { y, .. } => *y += dy,
        D::BgImage { y, py, .. } => {
            *y += dy;
            *py += dy;
        }
        D::PopClip => {}
        D::PushEffect(e) => e.offset_mut(0.0, dy),
        D::PopEffect => {} // the wrapped items' coords shift too — conjugate so painted
        // output moves by the same dy (T(d)·M·T(-d) ∘ p+d = M(p)+d)
        D::PushTransform { m } => *m = m.conjugated(0.0, dy),
        D::PopTransform => {}
    }
}

/// A device-pixel rect in viewport space: (x, y, w, h).
type Rect = (f32, f32, f32, f32);

fn rect_union(a: Rect, b: Rect) -> Rect {
    let x0 = a.0.min(b.0);
    let y0 = a.1.min(b.1);
    let x1 = (a.0 + a.2).max(b.0 + b.2);
    let y1 = (a.1 + a.3).max(b.1 + b.3);
    (x0, y0, x1 - x0, y1 - y0)
}

/// Dynamic-pseudo bits for `Page::pseudo_mask` — which state changes a
/// page's sheets could restyle. `P_FOCUS` also covers the UA sheet's
/// `input:focus,textarea:focus,select:focus` outline, so it is set even
/// when no author rule matches.
const P_HOVER: u8 = 1;
const P_FOCUS: u8 = 2;
const P_ACTIVE: u8 = 4;
const P_TARGET: u8 = 8;

/// Pseudo-classes one `Simple` selector term can match, recursing into
/// `:is`/`:where`/`:not`/`:has` argument lists.
fn simple_mask(s: &rowser_css::Simple) -> u8 {
    use rowser_css::Simple as S;
    match s {
        S::Hover => P_HOVER,
        S::Focus | S::FocusWithin | S::FocusVisible => P_FOCUS,
        S::Active => P_ACTIVE,
        S::Target => P_TARGET,
        S::Is(list) | S::Where(list) | S::Not(list) | S::Has(list) => list
            .iter()
            .fold(0, |m, sel| m | selector_mask(sel)),
        _ => 0,
    }
}

fn selector_mask(sel: &rowser_css::Selector) -> u8 {
    sel.parts
        .iter()
        .flat_map(|(_, simples)| simples.iter())
        .fold(0, |m, s| m | simple_mask(s))
}

/// Union of pseudo masks across a sheet's rules' selectors.
fn sheet_mask(sheet: &rowser_css::StyleSheet) -> u8 {
    sheet
        .rules
        .iter()
        .flat_map(|r| r.selectors.iter())
        .fold(0, |m, s| m | selector_mask(s))
}

/// Outcome of a display-list diff for damage purposes.
enum Damage {
    /// Lists are provably identical — the repaint can't differ.
    Clean,
    /// Diffs bounded by this rect.
    Region(Rect),
    /// Structural difference (item count, or a transform/effect whose
    /// footprint spans other items) — damage the whole viewport.
    All,
}

/// Painted-region diff of two display lists. Identical items paint
/// identically, so only the bounds of pairwise-differing items join the
/// damage — anything unbounded falls back to `All`.
fn diff_damage(
    old: &[rowser_layout::DisplayItem],
    new: &[rowser_layout::DisplayItem],
    fonts: &mut FontCache,
) -> Damage {
    if old.len() != new.len() {
        return Damage::All;
    }
    let mut acc: Option<Rect> = None;
    for (a, b) in old.iter().zip(new) {
        if items_eq(a, b) {
            continue;
        }
        match (item_bounds(a, fonts), item_bounds(b, fonts)) {
            (Some(ra), Some(rb)) => {
                let r = rect_union(ra, rb);
                acc = Some(acc.map_or(r, |acc| rect_union(acc, r)));
            }
            _ => return Damage::All,
        }
    }
    match acc {
        Some(r) => Damage::Region(r),
        None => Damage::Clean,
    }
}

/// Fieldwise display-item equality for damage diffing — a pair we can't
/// prove identical counts as different (more damage, never less).
fn items_eq(a: &rowser_layout::DisplayItem, b: &rowser_layout::DisplayItem) -> bool {
    use rowser_layout::DisplayItem as D;
    match (a, b) {
        (
            D::Text {
                x: ax,
                baseline: ab,
                text: at,
                font: af,
                size: a_s,
                color: ac,
                deco: ad,
                sel: a_sel,
                ls: al,
                href: a_h,
            },
            D::Text {
                x: bx,
                baseline: bb,
                text: bt,
                font: bf,
                size: b_s,
                color: bc,
                deco: bd,
                sel: b_sel,
                ls: bl,
                href: b_h,
            },
        ) => {
            ax == bx
                && ab == bb
                && at == bt
                && af == bf
                && a_s == b_s
                && ac == bc
                && a_sel == b_sel
                && al == bl
                && a_h == b_h
                && ad.lines == bd.lines
                && ad.style == bd.style
                && ad.color == bd.color
                && ad.thick == bd.thick
                && ad.offset == bd.offset
        }
        (
            D::FillRect {
                x: ax,
                y: ay,
                w: aw,
                h: ah,
                color: ac,
            },
            D::FillRect {
                x: bx,
                y: by,
                w: bw,
                h: bh,
                color: bc,
            },
        ) => ax == bx && ay == by && aw == bw && ah == bh && ac == bc,
        (
            D::RoundedRect {
                x: ax,
                y: ay,
                w: aw,
                h: ah,
                radius: ar,
                color: ac,
            },
            D::RoundedRect {
                x: bx,
                y: by,
                w: bw,
                h: bh,
                radius: br,
                color: bc,
            },
        ) => ax == bx && ay == by && aw == bw && ah == bh && ar == br && ac == bc,
        (
            D::RoundedRect2 {
                x: ax,
                y: ay,
                w: aw,
                h: ah,
                radii: ar,
                color: ac,
            },
            D::RoundedRect2 {
                x: bx,
                y: by,
                w: bw,
                h: bh,
                radii: br,
                color: bc,
            },
        ) => ax == bx && ay == by && aw == bw && ah == bh && ar == br && ac == bc,
        (
            D::BorderSide {
                x: ax,
                y: ay,
                w: aw,
                h: ah,
                side: a_s,
                style: a_st,
                color: ac,
            },
            D::BorderSide {
                x: bx,
                y: by,
                w: bw,
                h: bh,
                side: b_s,
                style: b_st,
                color: bc,
            },
        ) => {
            ax == bx && ay == by && aw == bw && ah == bh && a_s == b_s && a_st == b_st && ac == bc
        }
        (
            D::Shadow {
                x: ax,
                y: ay,
                w: aw,
                h: ah,
                dx: adx,
                dy: ady,
                blur: ab,
                spread: a_s,
                radii: ar,
                color: ac,
                inset: ai,
            },
            D::Shadow {
                x: bx,
                y: by,
                w: bw,
                h: bh,
                dx: bdx,
                dy: bdy,
                blur: bb,
                spread: b_s,
                radii: br,
                color: bc,
                inset: bi,
            },
        ) => {
            ax == bx
                && ay == by
                && aw == bw
                && ah == bh
                && adx == bdx
                && ady == bdy
                && ab == bb
                && a_s == b_s
                && ar == br
                && ac == bc
                && ai == bi
        }
        (
            D::Raster {
                x: ax,
                y: ay,
                w: aw,
                h: ah,
                pixels: ap,
            },
            D::Raster {
                x: bx,
                y: by,
                w: bw,
                h: bh,
                pixels: bp,
            },
        ) => ax == bx && ay == by && aw == bw && ah == bh && ap == bp,
        (
            D::Image {
                x: ax,
                y: ay,
                w: aw,
                h: ah,
                src: a_s,
                near: an,
            },
            D::Image {
                x: bx,
                y: by,
                w: bw,
                h: bh,
                src: b_s,
                near: bn,
            },
        ) => ax == bx && ay == by && aw == bw && ah == bh && a_s == b_s && an == bn,
        (
            D::BgImage {
                x: ax,
                y: ay,
                w: aw,
                h: ah,
                px: apx,
                py: apy,
                pw: apw,
                ph: aph,
                mode: am,
                sw: asw,
                sh: ash,
                fx: afx,
                ax: aax,
                fy: afy,
                ay: aay,
                rx: arx,
                ry: ary,
                src: a_s,
                near: an,
            },
            D::BgImage {
                x: bx,
                y: by,
                w: bw,
                h: bh,
                px: bpx,
                py: bpy,
                pw: bpw,
                ph: bph,
                mode: bm,
                sw: bsw,
                sh: bsh,
                fx: bfx,
                ax: bax,
                fy: bfy,
                ay: bay,
                rx: brx,
                ry: bry,
                src: b_s,
                near: bn,
            },
        ) => {
            ax == bx
                && ay == by
                && aw == bw
                && ah == bh
                && apx == bpx
                && apy == bpy
                && apw == bpw
                && aph == bph
                && am == bm
                && asw == bsw
                && ash == bsh
                && afx == bfx
                && aax == bax
                && afy == bfy
                && aay == bay
                && arx == brx
                && ary == bry
                && a_s == b_s
                && an == bn
        }
        (
            D::PushClip {
                x: ax,
                y: ay,
                w: aw,
                h: ah,
                radii: ar,
            },
            D::PushClip {
                x: bx,
                y: by,
                w: bw,
                h: bh,
                radii: br,
            },
        ) => ax == bx && ay == by && aw == bw && ah == bh && ar == br,
        (D::PushTransform { m: am }, D::PushTransform { m: bm }) => am == bm,
        (D::PopClip, D::PopClip) | (D::PopEffect, D::PopEffect) | (D::PopTransform, D::PopTransform) => {
            true
        }
        (D::PushEffect(ae), D::PushEffect(be)) => format!("{ae:?}") == format!("{be:?}"),
        _ => false,
    }
}

/// Painted bounds of a display item (viewport space). `None` for
/// structural items — effects/transforms/clips influence other items'
/// footprints, so a diff there damages the whole viewport.
fn item_bounds(it: &rowser_layout::DisplayItem, fonts: &mut FontCache) -> Option<Rect> {
    use rowser_layout::DisplayItem as D;
    match it {
        D::FillRect { x, y, w, h, .. }
        | D::RoundedRect { x, y, w, h, .. }
        | D::RoundedRect2 { x, y, w, h, .. }
        | D::BorderSide { x, y, w, h, .. }
        | D::Raster { x, y, w, h, .. }
        | D::Image { x, y, w, h, .. }
        | D::PushClip { x, y, w, h, .. }
        | D::BgImage { x, y, w, h, .. } => Some((*x, *y, *w, *h)),
        D::Shadow {
            x,
            y,
            w,
            h,
            dx,
            dy,
            blur,
            spread,
            ..
        } => {
            let m = blur * 3.0 + spread.abs();
            let x0 = x.min(*x + *dx) - m;
            let y0 = y.min(*y + *dy) - m;
            let x1 = (*x + *w).max(*x + *dx + *w) + m;
            let y1 = (*y + *h).max(*y + *dy + *h) + m;
            Some((x0, y0, x1 - x0, y1 - y0))
        }
        D::Text {
            x,
            baseline,
            text,
            font,
            size,
            ls,
            ..
        } => {
            let (top, bot, w) = match fonts.font(*font) {
                Some(f) => {
                    let ascent = f.ascent(*size);
                    (
                        *baseline - ascent,
                        *baseline - ascent + f.line_height(*size),
                        f.measure(text, *size) + ls * text.chars().count() as f32,
                    )
                }
                None => (
                    *baseline - *size,
                    *baseline + *size * 0.4,
                    text.chars().count() as f32 * size * 0.6,
                ),
            };
            // a half-em of overhang covers ink escaping the advance box
            Some((*x, top, w + size * 0.5, bot - top))
        }
        D::PushEffect(_)
        | D::PopEffect
        | D::PopClip
        | D::PushTransform { .. }
        | D::PopTransform => None,
    }
}

/// Send a frame if damage is pending — a full `Frame` when the chrome's
/// buffer needs it, a `FrameRegion` for a sub-rect, nothing at all when
/// the painted pixels are unchanged.
fn send_frame(w: &mut impl Write, page: &mut Page) {
    if let Some(m) = page.take_frame() {
        let _ = send(w, &m);
    }
}

/// Resolve a URL to HTML text + the effective URL (after fallback).
/// Gather @font-face rules from the page's sheets (linked + inline
/// <style>), fetch usable sources, and register them in the FontCache so
/// `font-family` resolves to web fonts. Web faces from the previous page
/// are deactivated first — the set is per-document.
fn load_web_fonts(page: &Page, fonts: &mut FontCache) {
    fonts.clear_registered();
    let mut faces: Vec<rowser_css::FontFace> = Vec::new();
    for s in &page.sheets {
        faces.extend(s.fonts.iter().cloned());
    }
    if let Some(doc) = &page.doc {
        for s in rowser_css::document_sheets_vw(doc, page.viewport.0 as f32) {
            faces.extend(s.fonts);
        }
    }
    if faces.is_empty() {
        return;
    }
    let t = std::time::Instant::now();
    fontface::register_font_faces(&faces, &page.url_base, fonts);
    pt!(
        "[prof] webfonts {} declared faces in {:?}",
        faces.len(),
        t.elapsed()
    );
}

fn load_url(url: &str) -> (String, String) {
    if let Some(rest) = url.strip_prefix("file://") {
        let path = rowser_net::percent_decode(rest).unwrap_or_else(|_| rest.to_string());
        return match std::fs::read_to_string(&path) {
            Ok(html) => (html, url.to_string()),
            Err(e) => (
                error_page(url, &format!("cannot read {path}: {e}")),
                url.to_string(),
            ),
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
                rowser_html::decode(&res.body, res.charset().as_deref()),
                res.url.to_string(),
            ),
            Err(e) => (
                error_page(url, &format!("network error: {e}")),
                url.to_string(),
            ),
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
) -> (
    rowser_dom::Document,
    Vec<String>,
    Option<std::sync::mpsc::Sender<JsJob>>,
) {
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
        let ty = doc
            .attr(id, "type")
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
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
        return (doc, Vec::new(), None);
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
    // The interpreter owns the live document for the page's lifetime: it
    // reports an initial snapshot after scripts run, then services DOM
    // event jobs (clicks) until the channel closes.
    let (job_tx, job_rx) = std::sync::mpsc::channel::<JsJob>();
    let (init_tx, init_rx) = std::sync::mpsc::channel::<(rowser_dom::Document, Vec<String>)>();
    let (vw, vh) = (viewport.0 as f64, viewport.1 as f64);
    let url = page_url.to_string();
    let spawned = std::thread::Builder::new()
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
            if init_tx.send((dc.snapshot_document(), logs)).is_err() {
                return;
            }
            // Event loop: each job dispatches a DOM event into the live
            // context and replies with the mutated snapshot + new logs.
            while let Ok(job) = job_rx.recv() {
                match job {
                    JsJob::Event {
                        nid,
                        ty,
                        client,
                        reply,
                    } => {
                        let mut logs = Vec::new();
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            if let Err(e) = dc.dispatch_event(nid, &ty, client) {
                                let msg = dc.ctx.interp.inspect(&e, 0);
                                logs.push(format!("[js] event {ty}: {msg}"));
                            }
                            dc.flush_raf();
                            dc.run_timers();
                        }));
                        for e in dc.errors.drain(..) {
                            logs.push(format!("[js] {e}"));
                        }
                        for line in dc.ctx.take_output() {
                            logs.push(format!("[js console] {line}"));
                        }
                        if reply
                            .send(JsReply {
                                doc: dc.snapshot_document(),
                                logs,
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
        });
    match spawned {
        Ok(_) => match init_rx.recv() {
            Ok((doc, logs)) => (doc, logs, Some(job_tx)),
            Err(_) => (
                rowser_html::parse("<html><body></body></html>"),
                vec!["[js] interpreter thread died before initial snapshot".to_string()],
                None,
            ),
        },
        Err(e) => (
            // doc moved into the (failed) spawn — render a fallback
            rowser_html::parse("<html><body></body></html>"),
            vec![format!("[js] could not spawn interpreter thread: {e}")],
            None,
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
            let v = if ((x / 32) + (y / 32)) % 2 == 0 {
                40u8
            } else {
                48u8
            };
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
                out.push_str(&format!(
                    "{pad}\"{}\"\n",
                    t.chars().take(60).collect::<String>()
                ));
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
    let file = format!(
        "{}/.config/rowser/{name}.txt",
        std::env::var("HOME").unwrap_or_default()
    );
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

    let queue = Arc::new(Mutex::new(urls.iter().cloned().collect::<VecDeque<_>>()));
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

/// `@import` expansion: each import's fetched sheet (and its own
/// imports, recursively, depth ≤ 4) splices in before the importing
/// sheet's rules — matching the spec's in-place insertion for imports
/// at the top of the sheet. `seen` dedupes/cycle-guards across sheets.
fn expand_imports(
    sheet: rowser_css::StyleSheet,
    base: &str,
    vw: f32,
    out: &mut Vec<rowser_css::StyleSheet>,
    depth: u8,
    seen: &mut std::collections::HashSet<String>,
) {
    if depth < 4 {
        let urls: Vec<String> = sheet
            .imports
            .iter()
            .filter(|i| rowser_css::media_query(&i.media, vw))
            .filter_map(|i| resolve_url(base, &i.url))
            .filter(|u| seen.insert(u.clone()))
            .collect();
        if !urls.is_empty() {
            let bodies = fetch_pool(&urls, CSS_FETCH_DEADLINE);
            for u in &urls {
                if let Some(b) = bodies.get(u) {
                    let sh = rowser_css::parse_vw(&String::from_utf8_lossy(b), vw);
                    expand_imports(sh, u, vw, out, depth + 1, seen);
                }
            }
        }
    }
    out.push(sheet);
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

/// One cookie store per renderer process (per tab) — seeded with
/// `SOCS=CAI` on .google.com to skip the EU consent interstitial. The
/// lock is only held for header lookup / Set-Cookie capture inside
/// `fetch_loop`, never across socket IO, so the subresource pool stays
/// parallel.
fn jar() -> &'static std::sync::Mutex<rowser_net::CookieJar> {
    static J: std::sync::OnceLock<std::sync::Mutex<rowser_net::CookieJar>> =
        std::sync::OnceLock::new();
    J.get_or_init(|| std::sync::Mutex::new(rowser_net::Session::with_google_consent().jar))
}

/// http/https fetch with redirects, cookies, keep-alive connection
/// reuse and TLS session resumption. Each calling thread owns a Session
/// — a connection pool is `!Sync`, so per-thread pools keep the
/// subresource fetch pool parallel — while all threads share `jar()`.
fn web_fetch(url: &str) -> io::Result<rowser_net::Response> {
    thread_local! {
        static SESS: std::cell::RefCell<rowser_net::Session> =
            std::cell::RefCell::new(rowser_net::Session::new());
    }
    SESS.with(|s| s.borrow_mut().fetch_with(url, jar()))
}


#[cfg(test)]
mod tests {
    use super::*;

    fn page_with(html: &str, url: &str) -> Page {
        let mut p = Page::new();
        p.doc = Some(rowser_html::parse(html));
        p.cur_url = url.to_string();
        p
    }

    fn first_form(p: &Page) -> rowser_dom::NodeId {
        let doc = p.doc.as_ref().unwrap();
        doc.descendants(doc.root)
            .find(|&n| doc.tag(n) == Some("form"))
            .unwrap()
    }

    #[test]
    fn form_urlencode_spec() {
        assert_eq!(form_urlencode("hello world"), "hello+world");
        assert_eq!(form_urlencode("a&b=c"), "a%26b%3Dc");
        assert_eq!(form_urlencode("über"), "%C3%BCber");
    }

    #[test]
    fn submit_get_collects_named_controls() {
        let mut p = page_with(
            r#"<form action="/html/" method="post">
                 <input type="text" name="q" value="rust lang">
                 <input type="hidden" name="kl" value="us-en">
                 <input type="submit" name="ignored" value="x">
                 <input name="unnamed-checked" type="checkbox" checked>
                 <input type="radio" name="r" value="a">
                 <input type="radio" name="r" value="b" checked>
               </form>"#,
            "https://duckduckgo.com/",
        );
        let f = first_form(&p);
        assert_eq!(
            p.submit_url(f).as_deref(),
            Some("https://duckduckgo.com/html/?q=rust+lang&kl=us-en&unnamed-checked=on&r=b")
        );
    }

    #[test]
    fn submit_default_action_is_current_url() {
        let p = page_with(
            r#"<form><input name="q" value="x"></form>"#,
            "https://example.com/search?old=1#f",
        );
        let f = first_form(&p);
        // action missing → current URL, old query replaced, fragment dropped
        assert_eq!(
            p.submit_url(f).as_deref(),
            Some("https://example.com/search?q=x")
        );
    }

    #[test]
    fn typing_edits_focused_input() {
        let mut p = page_with(
            r#"<form action="/s"><input name="q"></form>"#,
            "https://e.com/",
        );
        let doc = p.doc.as_ref().unwrap();
        let inp = doc
            .descendants(doc.root)
            .find(|&n| doc.tag(n) == Some("input"))
            .unwrap();
        p.set_focus(Some(inp));
        assert_eq!(p.edit, Some((inp, 0)));
        for c in ["h", "i"] {
            p.key(c, c, false);
        }
        assert_eq!(p.control_value(inp), "hi");
        p.key("backspace", "", false);
        assert_eq!(p.control_value(inp), "h");
        assert_eq!(p.edit, Some((inp, 1)));
        // Enter submits with the typed value
        match p.key("enter", "", false) {
            KeyOut::Navigate(u) => assert_eq!(u, "https://e.com/s?q=h"),
            _ => panic!("expected navigation"),
        }
    }

    #[test]
    fn caret_moves_inside_value() {
        let mut p = page_with(r#"<input name="q" value="abc">"#, "https://e.com/");
        let doc = p.doc.as_ref().unwrap();
        let inp = doc
            .descendants(doc.root)
            .find(|&n| doc.tag(n) == Some("input"))
            .unwrap();
        p.set_focus(Some(inp));
        assert_eq!(p.edit, Some((inp, 3)));
        p.key("left", "", false);
        p.key("a", "X", false);
        assert_eq!(p.control_value(inp), "abXc");
        assert_eq!(p.edit, Some((inp, 3)));
    }

    #[test]
    fn hidden_and_submit_inputs_not_editable() {
        let p = page_with(
            r#"<input type="hidden"><input type="submit"><input type="text"><textarea></textarea>"#,
            "https://e.com/",
        );
        let doc = p.doc.as_ref().unwrap();
        let ns: Vec<_> = doc
            .descendants(doc.root)
            .filter(|&n| matches!(doc.tag(n), Some("input") | Some("textarea")))
            .collect();
        assert!(!p.editable(ns[0])); // hidden
        assert!(!p.editable(ns[1])); // submit
        assert!(p.editable(ns[2])); // text
        assert!(p.editable(ns[3])); // textarea
    }

    #[test]
    fn submitter_detects_buttons() {
        let p = page_with(
            r#"<form><button type="submit"><b>go</b></button>
                 <input type="submit">
                 <button type="button"></button></form>"#,
            "https://e.com/",
        );
        let doc = p.doc.as_ref().unwrap();
        let b_inner = doc
            .descendants(doc.root)
            .find(|&n| doc.tag(n) == Some("b"))
            .unwrap();
        assert!(p.submitter(b_inner).is_some()); // climbs <b> → <button>
        let inp_submit = doc
            .descendants(doc.root)
            .find(|&n| doc.tag(n) == Some("input"))
            .unwrap();
        assert!(p.submitter(inp_submit).is_some());
        let btn_button = doc
            .descendants(doc.root)
            .filter(|&n| doc.tag(n) == Some("button"))
            .nth(1)
            .unwrap();
        assert!(p.submitter(btn_button).is_none()); // type=button
    }

    fn rect(x: f32, y: f32) -> rowser_layout::DisplayItem {
        rowser_layout::DisplayItem::FillRect {
            x,
            y,
            w: 10.0,
            h: 10.0,
            color: 0,
        }
    }

    #[test]
    fn diff_damage_tracks_changes() {
        let mut fonts = FontCache::new();
        let a = vec![rect(0.0, 0.0), rect(20.0, 0.0)];
        // identical lists paint identically — no damage
        assert!(matches!(
            diff_damage(&a, &a.clone(), &mut fonts),
            Damage::Clean
        ));
        // one moved item damages the union of old + new bounds
        let b = vec![rect(0.0, 0.0), rect(25.0, 5.0)];
        match diff_damage(&a, &b, &mut fonts) {
            Damage::Region((x, y, w, h)) => {
                assert_eq!((x, y, w, h), (20.0, 0.0, 15.0, 15.0));
            }
            _ => panic!("expected bounded region"),
        }
        // a changed item count is a structural change — whole viewport
        assert!(matches!(diff_damage(&a, &a[..1], &mut fonts), Damage::All));
    }

    #[test]
    fn pseudo_mask_scans_rules() {
        let none = rowser_css::parse("a { color: red } li:nth-child(2) { b: c }");
        assert_eq!(sheet_mask(&none), 0);
        let sh = rowser_css::parse(
            "a:hover{color:red} .x:focus-within{b:c} ul:not(:active){d:e} #t:target{f:g}",
        );
        assert_eq!(sheet_mask(&sh), P_HOVER | P_FOCUS | P_ACTIVE | P_TARGET);
    }
}
