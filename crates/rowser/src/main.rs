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
}

impl Tab {
    fn send(&mut self, msg: &ToRenderer) {
        if let Some(out) = &mut self.ipc_out {
            let _ = write_frame(out, &msg.encode());
        }
    }

    /// Navigate, recording the visit in history (drops forward entries).
    fn navigate(&mut self, url: &str) {
        self.url = url.to_string();
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
    inspector: bool,
    dom_dump: String,
}

impl BrowserView {
    fn new(cx: &mut Context<Self>) -> Self {
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
            inspector: false,
            dom_dump: String::new(),
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
        view
    }

    fn spawn_renderer() -> Option<(Child, UnixStream, UnixStream)> {
        let (parent, child_end) = UnixStream::pair().ok()?;
        let child_out = child_end.try_clone().ok()?;
        let exe = std::env::current_exe()
            .ok()?
            .with_file_name("rowser-renderer");
        let child = Command::new(exe)
            .stdin(Stdio::from(OwnedFd::from(child_end)))
            .stdout(Stdio::from(OwnedFd::from(child_out)))
            .spawn()
            .ok()?;
        Some((child, parent.try_clone().ok()?, parent))
    }

    fn open_tab(&mut self) {
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
                        // Switching tabs leaves URL-bar edit mode so the
                        // bar reflects the newly active tab.
                        view.url_editing = false;
                        view.url_anchor = None;
                        window.blur();
                        view.active = i;
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
                            let i = view.active;
                            let h = view.tabs[i].hist;
                            if h > 0 {
                                view.tabs[i].go(h - 1);
                                cx.notify();
                            }
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
                            let i = view.active;
                            let h = view.tabs[i].hist;
                            view.tabs[i].go(h + 1);
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
                            let i = view.active;
                            if view.tabs[i].alive {
                                let url = view.tabs[i].url.clone();
                                view.tabs[i].send(&ToRenderer::Navigate(url));
                            } else {
                                view.respawn_tab(i);
                            }
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
                                window.blur();
                                let i = view.active;
                                view.tabs[i].navigate(&url);
                            }
                            "escape" => {
                                view.url_editing = false;
                                view.url_anchor = None;
                                window.blur();
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
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view, e: &MouseDownEvent, window, cx| {
                        // Clicking the page leaves URL-bar edit mode.
                        view.url_editing = false;
                        view.url_anchor = None;
                        window.blur();
                        let p = e.position;
                        // webview starts below the 72px chrome; document space
                        // is device px (logical × scale factor)
                        let sf = window.scale_factor();
                        let i = view.active;
                        view.tabs[i].send(&ToRenderer::Click {
                            x: f32::from(p.x) * sf,
                            y: (f32::from(p.y) - CHROME_PX) * sf,
                        });
                        cx.notify();
                    }),
                )
                .on_scroll_wheel(cx.listener(|view, e: &ScrollWheelEvent, _window, cx| {
                    // GPUI reports content deltas (natural-scroll style):
                    // wheel up yields positive y. Our scroll offset is
                    // document-space, so negate it.
                    let dy = -f32::from(e.delta.pixel_delta(px(20.0)).y);
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
            |_, cx| cx.new(|cx| BrowserView::new(cx)),
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
