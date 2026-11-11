//! rowser: the browser process. A GPUI app that owns chrome UI and tabs,
//! spawning one `rowser-renderer` child per tab over a Unix socketpair.
//!
//! Renderers speak `rowser-ipc` on their stdin/stdout. A reader thread per
//! tab forwards `ToBrowser` messages into a shared mpsc channel; a GPUI task
//! drains it and repaints.

use gpui::{prelude::*, *};
use rowser_ipc::{read_frame, write_frame, ToBrowser, ToRenderer};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Height of the chrome (tab strip + nav bar) the webview doesn't get.
const CHROME_PX: f32 = 72.0;

enum TabEvent {
    Msg(ToBrowser),
    /// The renderer's socket closed — it crashed or exited.
    Disconnected,
}

struct Tab {
    id: usize,
    child: Option<Child>,
    ipc_out: Option<UnixStream>,
    frame: Option<Arc<RenderImage>>,
    title: String,
    url: String,
    alive: bool,
    history: Vec<String>,
    hist: usize,
    /// Page text currently selected in this tab's renderer (kept in sync
    /// by `ToBrowser::SelText`) — what Ctrl+C copies.
    sel_text: String,
}

impl Tab {
    fn send(&mut self, msg: &ToRenderer) {
        if let Some(out) = &mut self.ipc_out {
            let _ = write_frame(out, &msg.encode());
        }
    }

    /// Navigate, recording the visit in history (drops forward entries).
    fn navigate(&mut self, url: &str) {
        dlog(&format!("navigate {url}"));
        self.url = url.to_string();
        self.sel_text.clear();
        self.history.truncate(self.hist + 1);
        self.history.push(url.to_string());
        self.hist = self.history.len() - 1;
        record_history(url);
        self.send(&ToRenderer::Navigate(url.to_string()));
    }

    fn go(&mut self, idx: usize) {
        if idx < self.history.len() {
            self.hist = idx;
            self.url = self.history[idx].clone();
            self.sel_text.clear();
            self.send(&ToRenderer::Navigate(self.url.clone()));
        }
    }
}

struct BrowserView {
    tabs: Vec<Tab>,
    active: usize,
    next_id: usize,
    inbox: mpsc::Receiver<(usize, TabEvent)>,
    outbox: mpsc::Sender<(usize, TabEvent)>,
    last_viewport: (u32, u32),
    logs: Vec<String>,
    url_text: String,
    /// Caret position in `url_text`, as a *char* index.
    url_cursor: usize,
    /// Selection anchor (char index); `Some(anchor) != url_cursor` means the
    /// range between anchor and cursor is selected.
    url_anchor: Option<usize>,
    url_editing: bool,
    /// Focus the URL bar on the next render — set by `open_tab`, which runs
    /// without access to a `Window`.
    url_focus_pending: bool,
    url_focus: FocusHandle,
    /// Focused whenever the page (not the URL bar) should get keys —
    /// the root div tracks it so unfocused-window keys land on the
    /// shortcut handler instead of gpui's synthetic dispatch root.
    page_focus: FocusHandle,
    inspector: bool,
    dom_dump: String,
    /// In-progress page-text drag: the anchor point in document-space
    /// device px (set on mouse-down, cleared on mouse-up).
    drag: Option<(f32, f32)>,
    /// Cursor the page asked for (`ToBrowser::Cursor`) — applied while
    /// the pointer is over the webview.
    page_cursor: CursorStyle,
}

/// Map a CSS `cursor` keyword (renderer → `ToBrowser::Cursor`) to a GPUI
/// `CursorStyle`. Unknown/empty/auto keywords fall back to the arrow.
fn css_cursor(css: &str) -> CursorStyle {
    match css.trim() {
        "pointer" => CursorStyle::PointingHand,
        "text" => CursorStyle::IBeam,
        "vertical-text" => CursorStyle::IBeamCursorForVerticalLayout,
        "crosshair" => CursorStyle::Crosshair,
        "grab" => CursorStyle::OpenHand,
        "grabbing" => CursorStyle::ClosedHand,
        "not-allowed" => CursorStyle::OperationNotAllowed,
        "alias" => CursorStyle::DragLink,
        "copy" => CursorStyle::DragCopy,
        "context-menu" => CursorStyle::ContextualMenu,
        "w-resize" => CursorStyle::ResizeLeft,
        "e-resize" => CursorStyle::ResizeRight,
        "ew-resize" | "col-resize" => CursorStyle::ResizeLeftRight,
        "n-resize" => CursorStyle::ResizeUp,
        "s-resize" => CursorStyle::ResizeDown,
        "ns-resize" | "row-resize" => CursorStyle::ResizeUpDown,
        "nesw-resize" => CursorStyle::ResizeUpLeftDownRight,
        "nwse-resize" => CursorStyle::ResizeUpRightDownLeft,
        "none" => CursorStyle::None,
        _ => CursorStyle::Arrow,
    }
}

/// Append a timestamped line to the shared event log (renderer writes
/// the same file — lets us reconstruct clicks/keys/timings after the fact).
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
        let _ = writeln!(f, "{t:.3} [browser] {msg}");
    }
}

impl BrowserView {
    fn new(cx: &mut Context<Self>, initial_url: Option<String>) -> Self {
        let (outbox, inbox) = mpsc::channel::<(usize, TabEvent)>();
        let mut view = Self {
            tabs: Vec::new(),
            active: 0,
            next_id: 0,
            inbox,
            outbox,
            last_viewport: (1280, 720),
            logs: Vec::new(),
            url_text: "rowser://newtab/0".into(),
            url_cursor: 0,
            url_anchor: None,
            url_editing: false,
            url_focus_pending: false,
            url_focus: cx.focus_handle(),
            page_focus: cx.focus_handle(),
            inspector: false,
            dom_dump: String::new(),
            drag: None,
            page_cursor: CursorStyle::Arrow,
        };

        // Drain renderer messages into the UI. Polling via gpui::Timer keeps
        // this dep-free — no async channel crate needed.
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            loop {
                Timer::after(Duration::from_millis(4)).await;
                if this
                    .update(&mut *cx, |view, cx| {
                        view.drain_inbox();
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        view.open_tab();
        if let Some(url) = initial_url {
            let url = normalize_url(&url);
            view.url_text = url.clone();
            view.tabs[0].navigate(&url);
        }
        view
    }

    fn spawn_renderer() -> Option<(Child, UnixStream, UnixStream)> {
        let (parent, child_end) = UnixStream::pair().ok()?;
        let child_out = child_end.try_clone().ok()?;
        let exe = std::env::current_exe()
            .ok()?
            .with_file_name("rowser-renderer");
        // Renderer stderr → a per-pid log so panics are diagnosable from a
        // GUI launch (where stderr is otherwise lost).
        let mut cmd = Command::new(exe);
        cmd.stdin(Stdio::from(OwnedFd::from(child_end)))
            .stdout(Stdio::from(OwnedFd::from(child_out)));
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/rowser-renderer-stderr.log")
        {
            cmd.stderr(Stdio::from(f));
        }
        let child = cmd.spawn().ok()?;
        Some((child, parent.try_clone().ok()?, parent))
    }

    fn open_tab(&mut self) {
        dlog("open_tab (ctrl+t / +)");
        let id = self.next_id;
        self.next_id += 1;
        let url = format!("rowser://newtab/{id}");

        let mut tab = Tab {
            id,
            child: None,
            ipc_out: None,
            frame: None,
            title: "New Tab".into(),
            url: url.clone(),
            alive: false,
            history: vec![url.clone()],
            hist: 0,
            sel_text: String::new(),
        };

        if let Some((child, out, reader)) = Self::spawn_renderer() {
            tab.child = Some(child);
            tab.ipc_out = Some(out);
            tab.alive = true;
            let tx = self.outbox.clone();
            thread::spawn(move || reader_loop(id, reader, tx));
            tab.send(&ToRenderer::ResizeViewport {
                width: self.last_viewport.0,
                height: self.last_viewport.1,
            });
            tab.navigate(&url);
        } else {
            tab.title = "spawn failed".into();
        }

        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;

        // A new tab puts its whole URL in the bar pre-selected, so typing
        // immediately replaces it — like Ctrl+L + type in a real browser.
        // `open_tab` has no `Window`, so the focus itself is deferred to
        // `render` via `url_focus_pending`.
        self.url_text = url;
        self.url_editing = true;
        self.url_select_all();
        self.url_focus_pending = true;
    }

    // --- URL bar text editing -------------------------------------------
    // Cursor/anchor are char indices into `url_text`, so multi-byte chars
    // in a URL can't split mid-codepoint.

    fn url_len(&self) -> usize {
        self.url_text.chars().count()
    }

    /// Byte offset of char index `i` in `url_text`, clamped to the end.
    fn url_byte(&self, i: usize) -> usize {
        self.url_text
            .char_indices()
            .nth(i)
            .map(|(b, _)| b)
            .unwrap_or(self.url_text.len())
    }

    /// Selected char range `(start, end)`, end exclusive — `None` when the
    /// caret is collapsed.
    fn url_selection(&self) -> Option<(usize, usize)> {
        let a = self.url_anchor?;
        if a == self.url_cursor {
            None
        } else {
            Some((a.min(self.url_cursor), a.max(self.url_cursor)))
        }
    }

    fn url_select_all(&mut self) {
        self.url_anchor = Some(0);
        self.url_cursor = self.url_len();
    }

    /// Delete the selected range if there is one. Returns whether a
    /// selection existed.
    fn url_delete_selection(&mut self) -> bool {
        let Some((s, e)) = self.url_selection() else {
            self.url_anchor = None;
            return false;
        };
        self.url_text.drain(self.url_byte(s)..self.url_byte(e));
        self.url_cursor = s;
        self.url_anchor = None;
        true
    }

    /// Insert text at the caret, replacing any selection.
    fn url_insert(&mut self, s: &str) {
        self.url_delete_selection();
        let b = self.url_byte(self.url_cursor);
        self.url_text.insert_str(b, s);
        self.url_cursor += s.chars().count();
    }

    /// Move the caret by `delta` chars. With shift the selection is
    /// extended; without, a selection collapses to its near/far edge.
    fn url_move(&mut self, delta: isize, shift: bool) {
        let len = self.url_len() as isize;
        let next = (self.url_cursor as isize + delta).clamp(0, len) as usize;
        if shift {
            self.url_anchor.get_or_insert(self.url_cursor);
            self.url_cursor = next;
        } else if let Some((s, e)) = self.url_selection() {
            self.url_cursor = if delta < 0 { s } else { e };
            self.url_anchor = None;
        } else {
            self.url_cursor = next;
        }
    }

    /// Jump the caret to `pos` (clamped); shift extends the selection.
    fn url_jump(&mut self, pos: usize, shift: bool) {
        let pos = pos.min(self.url_len());
        if shift {
            self.url_anchor.get_or_insert(self.url_cursor);
        } else {
            self.url_anchor = None;
        }
        self.url_cursor = pos;
    }

    fn respawn_tab(&mut self, idx: usize) {
        let id = self.next_id;
        self.next_id += 1;
        let url = self.tabs[idx].url.clone();
        let mut fresh = Tab {
            id,
            child: None,
            ipc_out: None,
            frame: None,
            title: "New Tab".into(),
            url: url.clone(),
            alive: false,
            history: vec![url.clone()],
            hist: 0,
            sel_text: String::new(),
        };
        if let Some((child, out, reader)) = Self::spawn_renderer() {
            fresh.child = Some(child);
            fresh.ipc_out = Some(out);
            fresh.alive = true;
            let tx = self.outbox.clone();
            thread::spawn(move || reader_loop(id, reader, tx));
            fresh.send(&ToRenderer::ResizeViewport {
                width: self.last_viewport.0,
                height: self.last_viewport.1,
            });
            fresh.history = self.tabs[idx].history.clone();
            fresh.hist = self.tabs[idx].hist;
            fresh.send(&ToRenderer::Navigate(url));
        }
        let mut old = std::mem::replace(&mut self.tabs[idx], fresh);
        old.send(&ToRenderer::Shutdown);
        if let Some(mut c) = old.child.take() {
            let _ = c.kill();
        }
    }

    fn close_tab(&mut self, idx: usize) {
        dlog(&format!("close_tab {idx}"));
        let mut tab = self.tabs.remove(idx);
        tab.send(&ToRenderer::Shutdown);
        if let Some(mut c) = tab.child.take() {
            let _ = c.kill();
        }
        if self.tabs.is_empty() {
            self.open_tab();
        } else {
            self.active = self.active.min(self.tabs.len() - 1);
        }
    }

    // --- Tab/page ops shared by buttons and keyboard shortcuts -----------

    /// Switch to tab `idx`, leaving URL-bar edit mode so the bar reflects
    /// the newly active tab.
    fn activate_tab(&mut self, idx: usize, window: &mut Window) {
        if idx >= self.tabs.len() {
            return;
        }
        self.url_editing = false;
        self.url_anchor = None;
        self.page_focus.focus(window);
        self.active = idx;
    }

    /// Step back/forward in the active tab's history (the ◀ ▶ buttons).
    fn history_step(&mut self, delta: isize) {
        let i = self.active;
        let next = self.tabs[i].hist as isize + delta;
        if next >= 0 {
            self.tabs[i].go(next as usize); // go() bounds-checks the top end
        }
    }

    /// Reload the active tab by re-navigating to its current URL (the ⟳
    /// button); a dead renderer is respawned instead.
    fn reload_active(&mut self) {
        let i = self.active;
        if self.tabs[i].alive {
            let url = self.tabs[i].url.clone();
            self.tabs[i].send(&ToRenderer::Navigate(url));
        } else {
            self.respawn_tab(i);
        }
    }

    /// Focus the URL bar with its contents selected — Ctrl+L. A typed
    /// in-progress edit is kept; otherwise the active tab's URL is shown.
    fn focus_url_bar(&mut self, window: &mut Window) {
        if !self.url_editing {
            self.url_text = self.tabs[self.active].url.clone();
        }
        self.url_editing = true;
        self.url_select_all();
        self.url_focus.focus(window);
    }

    fn drain_inbox(&mut self) {
        while let Ok((id, event)) = self.inbox.try_recv() {
            let is_active = self.tabs.get(self.active).is_some_and(|t| t.id == id);
            let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) else {
                continue;
            };
            match event {
                TabEvent::Msg(msg) => match msg {
                    ToBrowser::Pong(_) => {}
                    ToBrowser::LoadStarted => tab.title = "Loading…".into(),
                    ToBrowser::Title(t) => {
                        if t.is_empty() {
                            tab.title = tab.url.clone();
                        } else {
                            tab.title = t;
                        }
                        tab.send(&ToRenderer::DomDump);
                        if !self.url_editing && is_active {
                            self.url_text = tab.url.clone();
                        }
                    }
                    ToBrowser::Frame {
                        width,
                        height,
                        pixels,
                    } => {
                        if let Some(buf) = image::RgbaImage::from_raw(width, height, pixels) {
                            tab.frame = Some(Arc::new(RenderImage::new([image::Frame::new(buf)])));
                        }
                    }
                    ToBrowser::LoadDone => {}
                    ToBrowser::NavigationRequest(url) => tab.navigate(&url),
                    ToBrowser::DomDump(text) => {
                        if self.inspector {
                            self.dom_dump = text;
                        }
                    }
                    ToBrowser::SelText(t) => tab.sel_text = t,
                    ToBrowser::Cursor(css) => self.page_cursor = css_cursor(&css),
                    ToBrowser::Log(m) => self.logs.push(format!("tab {id}: {m}")),
                },
                TabEvent::Disconnected => {
                    tab.alive = false;
                    tab.ipc_out = None;
                    tab.frame = None;
                    tab.title = "crashed".into();
                }
            }
        }
    }
}

fn reader_loop(id: usize, mut reader: UnixStream, tx: mpsc::Sender<(usize, TabEvent)>) {
    loop {
        match read_frame(&mut reader) {
            Ok(payload) => match ToBrowser::decode(&payload) {
                Ok(msg) => {
                    if tx.send((id, TabEvent::Msg(msg))).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = tx.send((id, TabEvent::Msg(ToBrowser::Log("bad frame".into()))));
                }
            },
            Err(_) => {
                let _ = tx.send((id, TabEvent::Disconnected));
                break;
            }
        }
    }
}

impl Render for BrowserView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Deferred URL-bar focus from `open_tab` — it runs without a
        // `Window`, so the flag is consumed here on the next frame.
        if self.url_focus_pending {
            self.url_focus_pending = false;
            self.url_focus.focus(window);
        }

        // Propagate viewport changes to every live renderer. Frames are
        // painted at *device* pixels (logical size × scale factor) so text
        // isn't bilinearly blurred when GPUI composites the bitmap.
        let size = window.viewport_size();
        let sf = window.scale_factor();
        let content_w = (f32::from(size.width) * sf).max(64.0) as u32;
        let content_h = ((f32::from(size.height) - CHROME_PX) * sf).max(64.0) as u32;
        if (content_w, content_h) != self.last_viewport {
            self.last_viewport = (content_w, content_h);
            for tab in &mut self.tabs {
                tab.send(&ToRenderer::ResizeViewport {
                    width: content_w,
                    height: content_h,
                });
            }
        }

        // Tab strip
        let mut tab_strip = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_2()
            .h(px(32.))
            .bg(rgb(0x1b1b1f))
            .overflow_x_hidden();
        for (i, tab) in self.tabs.iter().enumerate() {
            let is_active = i == self.active;
            let mut chip = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .h_full()
                .min_w(px(80.))
                .max_w(px(220.))
                .text_sm()
                .text_color(if is_active {
                    rgb(0xffffff)
                } else {
                    rgb(0x888899)
                })
                .bg(if is_active { rgb(0x2e2e36) } else { rgb(0x1b1b1f) })
                .rounded_t_md()
                .overflow_hidden()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, _e, window, _cx| {
                        view.activate_tab(i, window);
                    }),
                )
                .child(div().flex_1().overflow_hidden().child(if tab.alive {
                    tab.title.clone()
                } else {
                    format!("⚠ {}", tab.title)
                }));
            if self.tabs.len() > 1 {
                chip = chip.child(
                    div()
                        .px_1()
                        .rounded_sm()
                        .text_color(rgb(0x777788))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |view, _e, _w, cx| {
                                view.close_tab(i);
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        )
                        .child("×"),
                );
            }
            tab_strip = tab_strip.child(chip);
        }
        tab_strip = tab_strip.child(
            div()
                .px_2()
                .text_lg()
                .text_color(rgb(0xaaaacc))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view, _e, _w, cx| {
                        view.open_tab();
                        cx.notify();
                    }),
                )
                .child("+"),
        );

        // Nav bar
        let active = &self.tabs[self.active];
        // URL bar contents: while editing, draw the text in segments so the
        // selection is highlighted and the caret sits at `url_cursor`.
        let url_el = if self.url_editing {
            let len = self.url_len();
            let cur = self.url_cursor.min(len);
            let mut row = div().flex().flex_row().items_center();
            if let Some((s, e)) = self.url_selection() {
                let (bs, be) = (self.url_byte(s.min(len)), self.url_byte(e.min(len)));
                row = row.child(self.url_text[..bs].to_string());
                if cur == s {
                    row = row.child("▍");
                }
                row = row.child(
                    div()
                        .bg(rgb(0x2b5fb8))
                        .text_color(rgb(0xffffff))
                        .child(self.url_text[bs..be].to_string()),
                );
                if cur == e {
                    row = row.child("▍");
                }
                row = row.child(self.url_text[be..].to_string());
            } else {
                let bc = self.url_byte(cur);
                row = row.child(format!("{}▍{}", &self.url_text[..bc], &self.url_text[bc..]));
            }
            row.into_any_element()
        } else {
            div().child(active.url.clone()).into_any_element()
        };
        let starred = bookmarks().iter().any(|b| *b == active.url);
        let alive = active.alive;
        let nav = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .h(px(40.))
            .bg(rgb(0x24242a))
            .child(
                div()
                    .px_2()
                    .rounded_md()
                    .text_color(if self.tabs[self.active].hist > 0 {
                        rgb(0xdddddd)
                    } else {
                        rgb(0x555566)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, _e, _w, cx| {
                            view.history_step(-1);
                            cx.notify();
                        }),
                    )
                    .child("◀"),
            )
            .child(
                div()
                    .px_2()
                    .rounded_md()
                    .text_color(
                        if self.tabs[self.active].hist + 1 < self.tabs[self.active].history.len() {
                            rgb(0xdddddd)
                        } else {
                            rgb(0x555566)
                        },
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, _e, _w, cx| {
                            view.history_step(1);
                            cx.notify();
                        }),
                    )
                    .child("▶"),
            )
            .child(
                div()
                    .px_2()
                    .rounded_md()
                    .text_color(if alive { rgb(0xdddddd) } else { rgb(0x555566) })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, _e, _w, cx| {
                            view.reload_active();
                            cx.notify();
                        }),
                    )
                    .child("⟳"),
            )
            .child(
                div()
                    .flex_1()
                    .px_3()
                    .py_1()
                    .rounded_full()
                    .bg(rgb(0x151518))
                    .text_sm()
                    .text_color(rgb(0xcccccc))
                    .track_focus(&self.url_focus)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, _e, window, cx| {
                            view.url_focus.focus(window);
                            view.url_editing = true;
                            let i = view.active;
                            view.url_text = view.tabs[i].url.clone();
                            view.url_cursor = view.url_len();
                            view.url_anchor = None;
                            cx.notify();
                        }),
                    )
                    .on_key_down(cx.listener(|view, e: &KeyDownEvent, window, cx| {
                        let ks = &e.keystroke;
                        let shift = ks.modifiers.shift;
                        // Ctrl on Linux/Windows, Cmd on macOS.
                        let accel = ks.modifiers.control || ks.modifiers.platform;
                        match ks.key.as_str() {
                            "enter" => {
                                let url = normalize_url(&view.url_text);
                                view.url_editing = false;
                                view.url_anchor = None;
                                view.page_focus.focus(window);
                                let i = view.active;
                                view.tabs[i].navigate(&url);
                            }
                            "escape" => {
                                view.url_editing = false;
                                view.url_anchor = None;
                                view.page_focus.focus(window);
                            }
                            "backspace" => {
                                if !view.url_delete_selection() && view.url_cursor > 0 {
                                    let (s, e) = (
                                        view.url_byte(view.url_cursor - 1),
                                        view.url_byte(view.url_cursor),
                                    );
                                    view.url_text.drain(s..e);
                                    view.url_cursor -= 1;
                                }
                            }
                            "delete" => {
                                if !view.url_delete_selection() && view.url_cursor < view.url_len()
                                {
                                    let (s, e) = (
                                        view.url_byte(view.url_cursor),
                                        view.url_byte(view.url_cursor + 1),
                                    );
                                    view.url_text.drain(s..e);
                                }
                            }
                            "left" => view.url_move(-1, shift),
                            "right" => view.url_move(1, shift),
                            "home" => view.url_jump(0, shift),
                            "end" => view.url_jump(usize::MAX, shift),
                            "a" if accel => view.url_select_all(),
                            "c" | "x" if accel => {
                                if let Some((s, e)) = view.url_selection() {
                                    let text = view.url_text[view.url_byte(s)..view.url_byte(e)]
                                        .to_string();
                                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                                    if ks.key == "x" {
                                        view.url_delete_selection();
                                    }
                                }
                            }
                            "v" if accel => {
                                if let Some(text) =
                                    cx.read_from_clipboard().and_then(|item| item.text())
                                {
                                    view.url_insert(&text);
                                }
                            }
                            _ => {
                                if let Some(c) = &ks.key_char {
                                    if !ks.modifiers.control && !ks.modifiers.platform {
                                        view.url_insert(c);
                                    }
                                }
                            }
                        }
                        cx.notify();
                    }))
                    .child(url_el),
            )
            .child(
                div()
                    .px_2()
                    .rounded_md()
                    .text_color(if starred { rgb(0xffcc44) } else { rgb(0x666677) })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, _e, _w, cx| {
                            let i = view.active;
                            let url = view.tabs[i].url.clone();
                            toggle_bookmark(&url);
                            cx.notify();
                        }),
                    )
                    .child("★"),
            )
            .child(
                div()
                    .px_2()
                    .rounded_md()
                    .text_color(if self.inspector { rgb(0x66ccff) } else { rgb(0x666677) })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, _e, _w, cx| {
                            view.inspector = !view.inspector;
                            if view.inspector {
                                let i = view.active;
                                view.tabs[i].send(&ToRenderer::DomDump);
                            }
                            cx.notify();
                        }),
                    )
                    .child("◈"),
            );

        // Web view
        let active = &self.tabs[self.active];
        let webview = if !active.alive {
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .bg(rgb(0x101013))
                .child(
                    div()
                        .text_xl()
                        .text_color(rgb(0x888899))
                        .child("This tab's renderer crashed or was killed."),
                )
                .child(
                    div()
                        .px_4()
                        .py_2()
                        .rounded_md()
                        .bg(rgb(0x3a3af0))
                        .text_color(rgb(0xffffff))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|view, _e, _w, cx| {
                                let i = view.active;
                                view.respawn_tab(i);
                                cx.notify();
                            }),
                        )
                        .child("Respawn renderer"),
                )
        } else if let Some(frame) = &active.frame {
            div()
                .size_full()
                .cursor(self.page_cursor)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view, e: &MouseDownEvent, window, cx| {
                        // Clicking the page leaves URL-bar edit mode.
                        view.url_editing = false;
                        view.url_anchor = None;
                        view.page_focus.focus(window);
                        let p = e.position;
                        // webview starts below the 72px chrome; document space
                        // is device px (logical × scale factor)
                        let sf = window.scale_factor();
                        // Possible start of a text-selection drag — the
                        // click itself is deferred to mouse-up so a drag
                        // that begins on a link doesn't navigate.
                        let (dx, dy) = (
                            f32::from(p.x) * sf,
                            (f32::from(p.y) - CHROME_PX) * sf,
                        );
                        view.drag = Some((dx, dy));
                        let i = view.active;
                        view.tabs[i].send(&ToRenderer::MouseDown { x: dx, y: dy });
                        cx.notify();
                    }),
                )
                .on_mouse_move(cx.listener(|view, e: &MouseMoveEvent, window, cx| {
                    let sf = window.scale_factor();
                    let (mx, my) = (
                        f32::from(e.position.x) * sf,
                        (f32::from(e.position.y) - CHROME_PX) * sf,
                    );
                    let i = view.active;
                    // hover/active tracking — the renderer hit-tests and
                    // replies with a Cursor only when the value changes
                    view.tabs[i].send(&ToRenderer::MouseMove {
                        x: mx,
                        y: my,
                        down: e.pressed_button.is_some(),
                    });
                    if e.pressed_button != Some(MouseButton::Left) {
                        // Missed the up (released outside) — drop the drag.
                        view.drag = None;
                        return;
                    }
                    let Some((x0, y0)) = view.drag else { return };
                    view.tabs[i].send(&ToRenderer::Select {
                        x0,
                        y0,
                        x1: mx,
                        y1: my,
                    });
                    cx.notify();
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(|view, e: &MouseUpEvent, window, cx| {
                        let Some((x0, y0)) = view.drag.take() else { return };
                        let sf = window.scale_factor();
                        let (x1, y1) = (
                            f32::from(e.position.x) * sf,
                            (f32::from(e.position.y) - CHROME_PX) * sf,
                        );
                        let i = view.active;
                        if (x1 - x0).hypot(y1 - y0) < 4.0 * sf {
                            // Didn't move — a plain click, not a drag.
                            dlog(&format!("click x={x1:.0} y={y1:.0}"));
                            view.tabs[i].send(&ToRenderer::Click { x: x1, y: y1 });
                        } else {
                            // Final position may be past the last move event.
                            dlog(&format!("select-drag {x0:.0},{y0:.0} → {x1:.0},{y1:.0}"));
                            view.tabs[i].send(&ToRenderer::Select { x0, y0, x1, y1 });
                        }
                        cx.notify();
                    }),
                )
                .on_mouse_up_out(
                    MouseButton::Left,
                    cx.listener(|view, _e: &MouseUpEvent, _w, _cx| {
                        view.drag = None;
                    }),
                )
                .on_scroll_wheel(cx.listener(|view, e: &ScrollWheelEvent, _window, cx| {
                    // GPUI reports content deltas (natural-scroll style):
                    // wheel up yields positive y. Our scroll offset is
                    // document-space, so negate it.
                    let dy = -f32::from(e.delta.pixel_delta(px(20.0)).y);
                    dlog(&format!("scroll dy={dy:.0}"));
                    let i = view.active;
                    view.tabs[i].send(&ToRenderer::Scroll { dy });
                    cx.notify();
                }))
                .child(img(frame.clone()).size_full().object_fit(ObjectFit::Fill))
        } else {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(rgb(0x101013))
                .child(div().text_color(rgb(0x666677)).child("Waiting for frame…"))
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x101013))
            .track_focus(&self.page_focus)
            // Browser shortcuts. GPUI dispatches key events along the
            // focus path up to the root, so this fires both when nothing
            // is focused (page view) and — after the URL bar's own
            // handler — while the bar is focused. While editing, the
            // shortcuts that would collide with its caret/clipboard keys
            // (Alt+arrows) are skipped; tab ops and Ctrl+L still fire.
            .on_key_down(cx.listener(|view, e: &KeyDownEvent, window, cx| {
                let ks = &e.keystroke;
                let m = ks.modifiers;
                dlog(&format!("key {} mods c:{} a:{} s:{}", ks.key, m.control, m.alt, m.shift));
                // Ctrl on Linux/Windows, Cmd on macOS — same as the URL bar.
                let ctrl = m.control || m.platform;
                let editing = view.url_editing;
                match ks.key.as_str() {
                    "t" if ctrl && !m.alt && !m.shift => view.open_tab(),
                    "w" if ctrl && !m.alt && !m.shift => {
                        // Leave edit mode first so the bar reflects the
                        // next tab; closing the last tab opens a fresh
                        // one (which re-enters edit mode itself).
                        view.url_editing = false;
                        view.url_anchor = None;
                        view.page_focus.focus(window);
                        view.close_tab(view.active);
                    }
                    "l" if ctrl && !m.alt && !m.shift => view.focus_url_bar(window),
                    "c" if ctrl && !m.alt && !m.shift && !editing => {
                        // Copy the page-text selection (the URL bar's own
                        // handler covers Ctrl+C while editing).
                        let text = view.tabs[view.active].sel_text.clone();
                        if !text.is_empty() {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        }
                    }
                    "r" if ctrl && !m.alt => view.reload_active(),
                    "f5" if !m.alt => view.reload_active(),
                    "tab" if ctrl && !m.alt => {
                        let n = view.tabs.len();
                        let step = if m.shift { n - 1 } else { 1 };
                        view.activate_tab((view.active + step) % n, window);
                    }
                    "left" if m.alt && !ctrl && !editing => view.history_step(-1),
                    "right" if m.alt && !ctrl && !editing => view.history_step(1),
                    k if ctrl && !m.alt && !m.shift => {
                        // Ctrl+1..8 jumps to tab N; Ctrl+9 to the last tab.
                        if let Ok(n) = k.parse::<usize>() {
                            if (1..=9).contains(&n) {
                                let idx = if n == 9 { view.tabs.len() - 1 } else { n - 1 };
                                view.activate_tab(idx, window);
                            }
                        }
                    }
                    _ => {}
                }
                cx.notify();
            }))
            .child(tab_strip)
            .child(nav)
            .child({
                let mut row = div().flex_1().min_h_0().flex().child(webview.flex_1().min_w_0());
                if self.inspector {
                    let mut panel = div()
                        .w(px(320.0))
                        .h_full()
                        .flex_shrink_0()
                        .overflow_hidden()
                        .bg(rgb(0x0c0c10))
                        .border_l_1()
                        .border_color(rgb(0x333344))
                        .p_2()
                        .flex()
                        .flex_col();
                    for line in self.dom_dump.lines().take(600) {
                        panel = panel.child(
                            div()
                                .text_xs()
                                .whitespace_nowrap()
                                .text_color(rgb(0x88aa88))
                                .child(line.to_string()),
                        );
                    }
                    row = row.child(panel);
                }
                row
            })
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| {
                let url = std::env::args().nth(1);
                cx.new(|cx| BrowserView::new(cx, url))
            },
        )
        .unwrap();
        cx.activate(true);
    });
}


/// ~/.config/rowser, creating it on first use.
fn config_dir() -> PathBuf {
    let d = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/rowser");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn record_history(url: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config_dir().join("history.txt"))
    {
        use std::io::Write;
        let _ = writeln!(f, "{url}");
    }
}

fn bookmarks() -> Vec<String> {
    std::fs::read_to_string(config_dir().join("bookmarks.txt"))
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

fn toggle_bookmark(url: &str) {
    let path = config_dir().join("bookmarks.txt");
    let mut bm = bookmarks();
    if let Some(i) = bm.iter().position(|b| b == url) {
        bm.remove(i);
    } else {
        bm.push(url.to_string());
    }
    let _ = std::fs::write(path, bm.join("\n") + "\n");
}

/// Turn user-typed text into a URL: keep schemes, guess https for hosts,
/// treat existing paths as file://.
fn normalize_url(input: &str) -> String {
    let s = input.trim();
    if s.contains("://") || s == "about:blank" {
        return s.to_string();
    }
    if std::path::Path::new(s).exists() {
        return format!("file://{s}");
    }
    format!("https://{s}")
}

