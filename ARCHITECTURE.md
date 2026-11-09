# Rowser Architecture

A web browser written in Rust + GPUI. Everything the browser *engine* does —
parsing, layout, rasterization, networking, TLS, image codecs, text shaping —
is written from scratch. No code from Chromium, Gecko, Ladybird, Servo, or any
other browser. The only external dependencies are `gpui` itself (including its
transitive dependencies, which are GPUI's internals, not ours) and `std`.

## 0. Decisions (locked)

| Question | Decision |
|---|---|
| Ambition | Full browser eventually (HTML + CSS + JS) |
| Dependencies | Truly everything from scratch: std + GPUI only |
| First meaningful milestone | Render styled HTML in a window |
| Chrome features | Tabs + navigation, bookmarks & history, DOM inspector |
| Content rasterization | Decided below (§4): software rasterizer → bitmap → GPUI |
| JavaScript | Deferred completely; DOM designed with a clean script-engine seam |
| Process model | Multi-process from the start |
| Platforms | Linux only (Wayland/X11 via GPUI) |
| Content source | HTTPS early — hand-written TLS lands early, not last |
| Now | This document, then scaffold |

## 1. Design principles

- **Spec-guided, not spec-complete.** Where we implement a feature, we follow
  the WHATWG algorithm (HTML parsing, URL, CSS cascade) rather than inventing
  behavior. We do not claim conformance, and we stage aggressively.
- **One process per responsibility.** The UI never blocks on parsing, fetching,
  or a crashed renderer. A dead renderer kills one tab, not the browser.
- **Crate boundaries match engine boundaries.** Every crate is independently
  testable headlessly — the renderer crate can run in-process for tests.
- **No hidden dependencies.** Engine crates depend on `std` only. Anything std
  doesn't provide (shared memory, fd passing, seccomp) goes through `rowser-sys`
  using inline-asm syscalls — still zero crates.

## 2. Process model

```
┌─────────────────────────────────────────────┐
│ rowser (browser process — the GPUI app)     │
│  chrome UI: tab strip, URL bar, menus       │
│  bookmarks/history store, DOM inspector     │
│  composites renderer bitmaps, routes input  │
└───┬──────────────┬──────────────┬───────────┘
    │ socketpair   │ socketpair   │ socketpair
┌───▼────┐    ┌────▼───┐    ┌────▼───┐
│renderer│    │renderer│    │renderer│   rowser-renderer (one per tab)
│ tab 1  │    │ tab 2  │    │ tab 3  │
│ net·tls│    │        │    │        │
│ html·css│   │        │    │        │
│ layout │    │        │    │        │
│ raster │    │        │    │        │
└────────┘    └────────┘    └────────┘
```

- **rowser** — the single GPUI `Application`. Owns windows, tabs, the chrome
  UI, persistent storage, and a `TabManager` that supervises renderer children.
- **rowser-renderer** — a small binary embedding the whole engine. One instance
  per tab (site isolation — per-origin processes — is a later refinement).
- The renderer owns networking too. A separate network-service process (Chrome-
  style) is explicitly rejected for now: it adds a second IPC hop for zero
  benefit at our scale. The `rowser-net`/`rowser-tls` crates are written so
  they could move into a service process later.
- **Crash handling:** the browser monitors children via `waitpid` (through
  `std::process::Child`). A dead renderer → "tab crashed" page + a fresh
  renderer on reload.
- **Sandboxing (roadmap, not M0):** once renderers load real remote content,
  drop privileges inside the child via `seccomp`/`unshare` using hand-written
  syscalls in `rowser-sys`. Renderers are designed to need only: the IPC
  socket, font/image files (read-only), DNS, and TCP.

### Process bootstrap

`socketpair(AF_UNIX)`; the child inherits one end as **stdin+stdout**
(`Stdio::from(OwnedFd)`), so no fd-passing is needed to establish the control
channel — pure `std`. The renderer speaks IPC on fds 0/1; its logging goes to
stderr (inherited → terminal, or a log file).

## 3. IPC (`rowser-ipc`)

- **Framing:** `u32` length + binary payload over the stream socket.
- **Encoding:** hand-written — no serde. `rowser-ipc` provides a `Reader`/
  `Writer` byte cursor with `u32`/`u64`/`f64`/`String`/`Vec<T>` primitives;
  each message implements `Encode`/`Decode` manually (a `rowser-ipc-derive`
  proc-macro crate can come later — writing it is also from scratch).
- **Message sketch:**

  | Browser → Renderer | Renderer → Browser |
  |---|---|
  | `Navigate(url)` | `LoadStarted` / `LoadProgress(f)` / `LoadDone` |
  | `ResizeViewport(w,h)` | `Commit { title, url }` (post-redirect) |
  | `Mouse{Move,Down,Up,Wheel}` | `FrameReady { buffer }` + dirty rects |
  | `Key{Down,Up}` / `Ime` | `CursorChanged` / `HoverTarget(url?)` |
  | `ScrollDelta(d)` | `NavigationRequest(url)` (link click) |
  | `RequestDomSnapshot` | `DomSnapshot(serialized tree)` — inspector |
  | `Ping` / `Kill` | `Pong` / `Crashed(stage, msg)` |

- **Frame transport, v1:** pixels travel in `FrameReady` (one memcpy). At
  1080p BGRA that's ~8 MB/frame; Unix sockets do multiple GB/s, fine to start.
- **v2 (shared memory):** `rowser-sys` gains hand-rolled `mmap` and
  `memfd_create`/`sendmsg`+SCM_RIGHTS wrappers (std exposes none of these).
  Renderer writes into a shm segment; IPC carries only an fd + dirty rects.
- **Responsive scrolling trick:** on wheel events the browser immediately
  re-blit's the last frame at an offset, then the renderer repaints — the
  classic fast-scroll illusion, nearly free to implement.

## 4. Display path — the rasterization decision

**Chosen: software rasterizer inside the renderer → BGRA bitmap → IPC →
GPUI `img()` element.**

```rust
// browser process, inside the web-view Element::paint:
window.paint_image(bounds, corner_radii, self.frame.clone(), 0, false)?;
// or declaratively: img(ImageSource::Render(self.frame.clone()))
```

`gpui::RenderImage` wraps `image::Frame`, so the browser crate adds `image`
**as a type shim only** (already a GPUI transitive dep — we never decode with
it). `image::Frame::new(ImageBuffer::from_raw(w, h, bgra_bytes))`.

Why not display-list → GPUI primitives (the rejected alternative):

- Web text must be shaped by *our* engine eventually; routing text through
  GPUI's text system would be exactly the kind of "not from scratch" leak we
  ruled out — and a dead end for the real renderer.
- Process isolation is cleanest at the bitmap boundary; the renderer stays
  fully headless-testable.
- GPUI primitive semantics (gradients, blend modes, clipping) don't map 1:1 to
  CSS; writing our own rasterizer teaches us what the GPU version will need.

Escape hatch preserved: the rasterizer consumes a display list, and that list
is serializable — a future GPU paint path can consume the same list in the
browser process without touching the engine.

## 5. Workspace / crate map

```
rowser/                    cargo workspace
├── Cargo.toml             # workspace members only; gpui is the sole external dep
├── crates/
│   ├── rowser/            # BIN: GPUI app, chrome UI, tab mgr, storage   [gpui, image*]
│   ├── rowser-renderer/   # BIN: child process; hosts the engine        [std only]
│   ├── rowser-ipc/        # framing, Encode/Decode, message types       [std]
│   ├── rowser-sys/        # syscall wrappers: mmap, memfd, sendmsg, seccomp (later) [std + asm]
│   ├── rowser-url/        # WHATWG URL parser                           [std]
│   ├── rowser-net/        # HTTP/1.1 client, connection pool, cache     [std]
│   ├── rowser-tls/        # TLS 1.2/1.3 record layer, handshake         [std]
│   ├── rowser-crypto/     # SHA-2, HKDF, X25519, AES-GCM, ChaCha20-Poly1305 [std]
│   ├── rowser-compress/   # DEFLATE/zlib/gzip decoder (shared by HTTP + PNG) [std]
│   ├── rowser-dom/        # Node/Element/Document tree, tree ops        [std]
│   ├── rowser-html/       # WHATWG tokenizer + tree construction        [std]
│   ├── rowser-css/        # CSS tokenizer, parser, selectors            [std]
│   ├── rowser-style/      # selector matching, cascade, computed values [std]
│   ├── rowser-text/       # font scan, TTF/OTF parse, shaping, glyph raster [std]
│   ├── rowser-layout/     # box tree, block/inline layout, line breaking [std]
│   ├── rowser-image/      # PNG, then JPEG/GIF decoders                 [std]
│   ├── rowser-paint/      # display list + software rasterizer          [std]
│   └── rowser-js/         # STUB — future JS engine, seam only          [std]
```

\* `image` is a GPUI transitive dep used purely to wrap our BGRA bytes in
`RenderImage`; decode/encode APIs are never called. Everything else is `std`.

Dependency direction: `rowser-renderer` → engine crates; `rowser` (browser
process) → only `rowser-ipc`, `rowser-sys`, `gpui`. The engine never depends
on GPUI — enforced by the workspace graph, not by discipline.

## 6. Engine pipeline (inside a renderer)

```
Navigate(url)
  → fetch      rowser-url + rowser-net + rowser-tls   headers/body bytes
  → decode     charset sniff (UTF-8 first; others later)
  → parse      rowser-html tokenizer + tree builder → rowser-dom Document
  → styles     <style>/<link> → rowser-css → StyleSheet list
  → style      rowser-style: match + cascade + inherit → StyledNode tree
  → layout     rowser-layout: box tree → geometry     → BoxTree (fixed point in px)
  → text       rowser-text: fonts, shape runs, raster glyphs (during layout+paint)
  → paint      rowser-paint: display list → rasterize → BGRA buffer
  → ship       FrameReady → browser
```

Stage notes:

- **URL** — full WHATWG URL parser incl. percent-encoding, IDNA punted (ASCII
  hosts only initially). Needed by HTTP, `<a href>`, redirects, `<img src>`.
- **HTTP** — HTTP/1.1 only: keep-alive, chunked, `Content-Length`, gzip via
  `rowser-compress`, redirect chain, basic auth headers later. HTTP/2 much
  later (TLS ALPN will advertise only `http/1.1` until then).
- **DNS** — `std::net::ToSocketAddrs` covers it (std already wraps
  `getaddrinfo`); no hand-rolled resolver needed at first.
- **TLS** — staged: 1.3 first (X25519, `TLS_AES_128_GCM_SHA256` +
  `TLS_CHACHA20_POLY1305_SHA256`), then 1.2-ECDHE for older servers.
  X.509/DER parser + chain building; root store from `/etc/ssl/certs`.
  All primitives in `rowser-crypto`, verified against RFC test vectors.
  This is the largest single subsystem — it starts early, in parallel with
  layout, because HTTPS is the real web.
- **HTML** — the WHATWG tokenizer state machine and tree-construction
  insertion modes, implemented faithfully for the states we reach; foreign
  content (SVG/MathML) punted.
- **CSS** — CSS2.1 core + CSS3 selectors; cascade ordered by
  origin/specificity/order; `!important`; a pragmatic property subset grown
  per-milestone (colors, fonts, margins/padding/borders, display, position
  basic first).
- **Layout** — block + inline first; floats and tables next; flex/grid much
  later. Anonymous boxes, line boxes, intrinsic sizes — the hard parts are
  designed-in from the start even if implemented incrementally.
- **Text** — scan `/usr/share/fonts` (no fontconfig), parse TTF/OTF
  (`cmap`, `glyf`, `hmtx`, `head`, `kern`, later `GPOS`), Latin shaping
  (cmap → glyph ids → kerning → advance), scanline rasterization of
  quadratic outlines. Complex-script shaping and emoji are a later mountain.
- **Paint** — display list: `FillRect/Border/TextRun/Image/Clip/Blend` items;
  painter's-algorithm rasterization into a BGRA tile grid; dirty-rect
  tracking for partial repaints.

The JS seam: `rowser-dom` exposes a trait boundary (`DomApi`) the way a script
engine would consume it, and `<script>` elements are parsed but inert. That's
all the JS support that exists until the engine lands.

## 7. Browser-process subsystems

- **Chrome UI** — GPUI views: `TabStrip`, `UrlBar` (with scheme-aware
  rendering), `NavButtons`, `WebView` (the `img` element + input routing),
  `InspectorPanel` (tree view fed by `DomSnapshot`).
- **Tabs** — `TabManager { tab_id → Child, socket, last_frame, url, title }`;
  spawns/kills renderers, routes IPC, handles crash → sad-tab.
- **Bookmarks & history** — hand-rolled storage (no sqlite): append-only
  journal + in-memory index, fsync'd snapshots to `~/.local/share/rowser/`.
  Simple format, documented on disk.
- **Input** — key/mouse → IPC; IPC → cursor/hover updates back.

## 8. Testing strategy

- Each engine crate: unit tests headless in-process (renderer logic runs
  without being spawned).
- **html5lib-tests** data files for tokenizer/tree-construction correctness
  (test *data* — no code borrowed).
- **Crypto:** RFC known-answer vectors (X25519 RFC 7748, ChaCha-Poly1305
  RFC 8439, HKDF RFC 5869, AES-GCM NIST).
- **TLS:** live handshakes against real servers in CI-lite; our own toy
  server for determinism.
- **Rendering:** golden-image snapshots of our own rasterizer output on
  fixture pages; pixel-diff thresholds.
- **IPC:** loopback harness driving browser↔renderer in one test process.

## 9. Milestone roadmap

| M | Deliverable | Exit criteria |
|---|---|---|
| M0 | Scaffold: workspace, GPUI shell, spawn renderer, IPC echo, bitmap plumbing | Window shows a renderer-produced test pattern; tabs kill/respawn |
| M1 | file:// + HTML parse + text-only layout | Local HTML renders as readable text w/ headings |
| M2 | CSS parse + cascade + block/inline + colors/borders | **Styled-HTML milestone** — fixture page looks styled |
| M3 | Chrome: tabs, URL bar, back/forward, bookmarks/history, DOM inspector | Daily-driveable on local pages |
| M4 | HTTP/1.1 + deflate + PNG | `http://` pages load; images show |
| M5 | TLS 1.3 + X.509 verify | `https://` on a real site |
| M6 | floats, tables, `<img>` layout, JPEG/GIF | Real blogs/docs render credibly |
| M7+ | HTTP/2, shaping (GPOS/complex), sandbox, site isolation, then the JS engine | The long road |

## 10. Known risks / honest notes

- **Hand-written TLS is the riskiest item** — correctness *and* security.
  Mitigation: RFC vectors, staged cipher suites, and it's isolated behind
  `rowser-tls` so a vetted replacement could slot in if this ever stops being
  an exercise.
- **`std` gaps** (mmap, SCM_RIGHTS, seccomp, `waitid`): handled by
  `rowser-sys` inline-asm syscalls — small, auditable, Linux-only.
- **GPUI churn:** gpui 0.2.x is pre-1.0 and breaks APIs; we pin `=0.2.2`.
- **`image` crate shim** for `RenderImage`: flagged, minimal, never used for
  codec work. If a raw-pixel path appears in GPUI we drop it.
- **Text shaping scope:** Latin-only for a long time; a `GPOS`/complex-script
  engine is its own project deferred past M6.
- **Spec coverage vs. progress tension:** the tokenizer/cascade are written
  to the WHATWG algorithms where implemented, but we consciously build a
  subset — correctness over coverage.
