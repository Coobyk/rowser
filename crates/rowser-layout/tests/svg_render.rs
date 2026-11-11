//! SVG rasterization tests — synthetic cases always run; the real cburnett
//! piece set (fetched to /tmp/cb_*.svg by the task driver) is rasterized at
//! 90×90 and dumped as /tmp/cb_*.ppm for eyeballing (`magick x.ppm x.png`).

use rowser_dom::{Document, NodeId};

fn doc(svg: &str) -> (Document, NodeId) {
    let d = rowser_html::parse(svg);
    let s = d.find_deep(d.root, "svg").expect("svg element");
    (d, s)
}

fn raster(svg: &str, w: u32, h: u32, color: u32) -> Vec<u8> {
    let (d, s) = doc(svg);
    rowser_layout::svg::render(&d, s, w, h, color)
}

fn covered(px: &[u8]) -> usize {
    px.chunks(4).filter(|p| p[3] > 8).count()
}

fn lum_hist(px: &[u8]) -> (usize, usize) {
    let mut light = 0;
    let mut dark = 0;
    for p in px.chunks(4) {
        if p[3] < 128 {
            continue;
        }
        let l = (p[0] as u32 + p[1] as u32 + p[2] as u32) / 3;
        if l > 150 {
            light += 1;
        }
        if l < 80 {
            dark += 1;
        }
    }
    (light, dark)
}

/// BGRA-over-gray → PPM P6 at `path`.
fn write_ppm(path: &str, px: &[u8], w: usize, h: usize) {
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for p in px.chunks(4) {
        // straight-alpha composite over mid-gray
        let a = p[3] as u32;
        let (r, g, b) = (p[2] as u32, p[1] as u32, p[0] as u32);
        let bg = 0x80u32;
        out.push(((r * a + bg * (255 - a)) / 255) as u8);
        out.push(((g * a + bg * (255 - a)) / 255) as u8);
        out.push(((b * a + bg * (255 - a)) / 255) as u8);
    }
    std::fs::write(path, &out).expect("write ppm");
}

#[test]
fn lucide_icon_is_thin_lines_not_a_box() {
    let px = raster(
        r#"<svg fill="none" stroke="currentColor" viewBox="0 0 24 24" width="24" height="24"><path d="M5 12h14"/></svg>"#,
        24,
        24,
        0xffffffff,
    );
    let cov = covered(&px);
    assert!(cov > 8, "icon strokes some pixels, got {cov}");
    assert!(
        cov < 24 * 24 / 3,
        "icon must not be an opaque box, cov={cov}"
    );
    assert_eq!(px[3], 0, "corner pixel transparent");
}

#[test]
fn arc_heavy_piece_renders() {
    // cburnett-like: white fill, black round-joined stroke, arc + cubic paths
    let px = raster(
        r##"<svg viewBox="0 0 45 45"><g fill="#ffffff" stroke="#000000"
          stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
          <path d="M22.5 11.6a3 3 0 1 0 0.02 0z"/>
          <path d="M22.5 25 c-8 0 -10 -6 -10 -11 l0 -2 20 0 0 2 c0 5 -2 11 -10 11z"/>
          <path d="M12 34 a10.5 10.5 0 0 0 21 0 l0 -3 -21 0 z"/>
          <path d="M11 38.5 a35 35 1 0 0 23 0" fill="none"/>
        </g></svg>"##,
        90,
        90,
        0xff000000,
    );
    let cov = covered(&px);
    // geometry covers ~19% of the canvas — flag only parse failures (near-zero)
    assert!(cov > 1200, "piece coverage too low: {cov}");
    let (light, dark) = lum_hist(&px);
    assert!(
        light > 50 && dark > 50,
        "want light fill + dark stroke, got {light}/{dark}"
    );
}

#[test]
fn cburnett_pieces_if_fetched() {
    let pieces = [
        "wK", "wQ", "wR", "wB", "wN", "wP", "bK", "bQ", "bR", "bB", "bN", "bP",
    ];
    let mut any = false;
    for p in pieces {
        let Ok(svg) = std::fs::read_to_string(format!("/tmp/cb_{p}.svg")) else {
            continue;
        };
        any = true;
        let (d, s) = doc(&svg);
        let px = rowser_layout::svg::render(&d, s, 90, 90, 0xff000000);
        write_ppm(&format!("/tmp/cb_{p}.ppm"), &px, 90, 90);
        let cov = covered(&px);
        assert!(
            cov > 90 * 90 / 4,
            "{p}: coverage too low ({cov}) — likely a parse failure"
        );
        let (light, dark) = lum_hist(&px);
        // black pieces are pure silhouettes; only white pieces must show fill
        if p.starts_with('w') {
            assert!(light > 30, "{p}: no light pixels (white fill missing)");
        }
        assert!(dark > 30, "{p}: no dark pixels (black stroke/fill missing)");
        eprintln!(
            "{p}: cov={} light={} dark={}",
            cov * 100 / 8100,
            light,
            dark
        );
    }
    if !any {
        eprintln!(
            "no /tmp/cb_*.svg — fetch with: for p in wK wQ wR wB wN wP bK bQ bR bB bN bP; do \
                   curl -s https://goonchess.com/pieces/cburnett/$p.svg -o /tmp/cb_$p.svg; done"
        );
    }
}
