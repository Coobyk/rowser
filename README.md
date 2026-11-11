<img src="assets/icon.svg" width="64" height="64" alt="">

# Rowser

A web browser written from scratch in Rust — its own HTML parser, CSS
engine, layout engine, rasterizer, PNG decoder, HTTP client, TLS 1.3 stack,
and JavaScript interpreter. No code from Chromium, Gecko, Ladybird, or Servo.
The only third-party dependencies are [GPUI](https://gpui.rs) (window/compositor
shell for the chrome UI), `rustc-hash`, `swash` (text shaping), and the `image`
crate as a type shim for `gpui::RenderImage` — everything the *engine* does
is `std`-only.

Linux only (Wayland/X11 via GPUI). ~62k lines of Rust across 14 crates.

## Architecture

Multi-process: the `rowser` GPUI app owns the chrome UI and tabs, and spawns
one `rowser-renderer` child per tab over a Unix socketpair (hand-rolled
binary IPC, length-prefixed frames). The renderer fetches, parses, lays out,
and rasterizes pages to BGRA bitmaps which the browser composites. A dead
renderer kills one tab, not the browser — the UI offers a respawn button.

```
┌─ rowser (GPUI app) ─────────────────────────────┐
│ tab strip · URL bar · bookmarks/history · DOM   │
│ inspector · input routing · bitmap compositing  │
└───┬──────────────┬──────────────┬───────────────┘
    │ socketpair   │ socketpair   │ socketpair
┌───▼───┐     ┌────▼───┐     ┌────▼───┐
│renderer│    │renderer│     │renderer│  one per tab
│net·tls │    │        │     │        │
│html·css│    │        │     │        │
│layout  │    │        │     │        │
│paint·js│    │        │     │        │
└────────┘    └────────┘     └────────┘
```

Engine pipeline inside a renderer:

```
fetch (url/net/tls) → parse (html) → cascade (css) → layout (box tree,
flex/grid/table/float) → shape+paint (text/paint) → BGRA frame → IPC
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full design document,
decision log, and milestone roadmap.

## What's implemented

**Engine**

- **HTML** — WHATWG-shaped tokenizer + tree construction (mainline
  insertion modes, implied end tags, named/numeric char refs)
- **CSS** — Syntax L3 subset, Selectors 4 (`:is`/`:where`/`:not`/`:has`,
  `:nth-*`, `[attr op]` + `i`/`s` flags), full cascade (`!important`,
  `@layer`, `var()`/`@property`, wide keywords), dynamic states
  (`:hover`/`:focus`/`:active`/`:target`/`:link`), pseudo-elements
  (`::before`/`::after`/`::marker`/`::first-line`/`::first-letter`/
  `::selection`, counters), `@media`, `@font-face` end-to-end
- **Layout** — block + inline, floats, tables (auto/fixed, colspan/rowspan,
  border-collapse), flexbox, grid (auto-fill/fit, dense flow, intrinsic
  tracks, subgrid, named lines), multi-column, positioning
  (relative/absolute/fixed/sticky), 2D transforms
- **Paint** — software rasterizer → BGRA: gradients, background layers,
  `object-fit`, borders/radii/outlines, box-shadow, `filter`/`clip-path`/
  blend modes/backdrop effects, text selection highlight
- **Text** — system font scan (no fontconfig), TTF parsing + quadratic
  glyph rasterization (4×4 supersampled), shaping via `swash`, web fonts
- **Images** — PNG decoder (all color types, bit depths, Adam7, tRNS);
  inline SVG subset
- **Net** — HTTP/1.1 (keep-alive, chunked, gzip/deflate via hand-rolled
  RFC 1951 inflate), WHATWG URL parser, parallel subresource fetch pool
- **TLS** — TLS 1.3 client written from scratch: SHA-2, HMAC/HKDF,
  X25519, AES-GCM, ChaCha20-Poly1305, X.509/DER chain verification
  (RSA PKCS#1 v1.5, RSA-PSS, ECDSA P-256/P-384), system root store
- **JavaScript** — tree-walking interpreter (lexer/parser/evaluator,
  RegExp, builtins) with a DOM bridge: `document`, events, timers,
  `classList`, `innerHTML`, storage — page scripts actually run
  (`ROWSER_JS=0` disables them)

**Chrome**

- Tabs (one renderer process each) with crash detection + respawn
- URL bar with full editing/selection/clipboard; guesses `https://` for
  bare hosts, `file://` for existing paths
- Back/forward history, bookmarks, DOM inspector panel, page-text
  selection + copy, cursor propagation from the page
- Built-in pages: `rowser://newtab`, `rowser://bookmarks`, `rowser://history`

## Build & run

Requires Linux with a graphical session (Wayland or X11) and a recent
Rust toolchain.

```sh
cargo build            # both binaries must sit next to each other
./target/debug/rowser  # opens the browser
./target/debug/rowser https://example.com  # or with an initial URL
```

The browser locates `rowser-renderer` next to its own executable, so
`cargo run -p rowser` works too once `rowser-renderer` has been built
(`cargo build` builds the whole workspace). Release mode is noticeably
faster; the dev profile already compiles the engine's numeric hot paths
at `opt-level 3` so scrolling stays smooth in debug builds.

### Keys

| Key | Action |
|---|---|
| `Ctrl+T` / `Ctrl+W` | new / close tab |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | next / previous tab |
| `Ctrl+1..9` | jump to tab |
| `Ctrl+L` | focus URL bar |
| `Ctrl+R` / `F5` | reload (respawns a dead renderer) |
| `Alt+←` / `Alt+→` | back / forward |
| `Ctrl+C` | copy selected page text |

Toolbar: `◀ ▶ ⟳` nav, `★` bookmark toggle, `◈` DOM inspector, `+` new tab.

### Files

- `~/.config/rowser/history.txt` — visit history (drives `rowser://history`)
- `~/.config/rowser/bookmarks.txt` — bookmarks (`rowser://bookmarks`)
- `/tmp/rowser-events.log`, `/tmp/rowser-renderer-stderr.log` — debug logs

## Testing

```sh
cargo test --workspace --exclude rowser-tls   # engine + IPC tests
cargo test -p rowser-tls                      # crypto KATs (RFC vectors)
```

The TLS crate is exercised separately — crypto primitives are verified
against RFC/NIST known-answer vectors (X25519, ChaCha20-Poly1305, HKDF,
AES-GCM, SHA-2).

## Honest limitations

- Linux only; no sandboxing yet — don't point it at hostile content
- HTTP/1.1 only (no HTTP/2); `data:` URLs not implemented; JPEG/GIF not yet
- HTML parser defers adoption-agency and foster-parenting algorithms
- Text is Latin-focused; no complex-script shaping, CFF/OTF outlines, or hinting
- JS engine covers the language core + a practical DOM subset; it is not
  fast and not spec-complete
- Hand-written TLS is the riskiest component — audited against RFC vectors,
  but it has not had independent security review. Treat accordingly.
- The `xattr` crate is vendored under `vendor/` to fix a `gpui` transitive
  dep against modern libc (see root `Cargo.toml` `[patch]` section).

## License

MIT. Created in [T3 Code](https://t3.codes).
