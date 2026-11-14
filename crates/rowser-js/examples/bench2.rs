//! Synthetic hot-path benchmark for the JS engine + DOM bridge.
//!
//! Exercises the paths a real React app (e.g. goonchess.com) hammers:
//! `hasOwnProperty`/`Object.*`, array methods, `JSON.stringify`, element
//! wrapper `get_prop`/`set_prop`, `style`/`classList`/attrs, tree mutation,
//! event dispatch, and the microtask/timer/rAF scheduler.
//!
//! Run:  cargo run -p rowser-js --release --example bench2 [-- --iters N]

use std::time::Instant;

/// One iteration ≈ a small render + dispatch cycle.
const WORK: &str = r##"
(function () {
    var out = 0;
    // -- Object.* / hasOwnProperty ------------------------------------------
    var o = { a: 1, b: 2, c: 3, d: 4, e: 5 };
    for (var k = 0; k < 200; k++) {
        if (o.hasOwnProperty("a")) out += 1;
        if (Object.prototype.hasOwnProperty.call(o, "z")) out += 100;
        var ks = Object.keys(o);
        var vs = Object.values(o);
        var es = Object.entries(o);
        var t = Object.assign({}, o, { f: 6 });
        out += ks.length + vs.length + es.length + (t.f || 0);
    }
    // -- array methods --------------------------------------------------------
    var arr = [];
    for (var n = 0; n < 50; n++) arr.push(n);
    for (var m = 0; m < 100; m++) {
        out += arr.map(function (x) { return x * 2; })
            .filter(function (x) { return x % 3 === 0; })
            .reduce(function (a, b) { return a + b; }, 0);
        out += arr.join(",").length;
        out += arr.indexOf(25) + arr.lastIndexOf(10) + arr.slice(5, 20).length;
        if (arr.includes(42)) out += 1;
        out += arr.at(-1);
    }
    // -- JSON ---------------------------------------------------------------
    var payload = { items: arr, meta: o, tag: "bench", nested: { x: [1, 2, { y: "z" }] } };
    var js = "";
    for (var j = 0; j < 100; j++) js = JSON.stringify(payload);
    out += js.length + JSON.parse(js).items.length;
    // -- DOM element wrappers -------------------------------------------------
    var root = document.getElementById("root");
    var parent = document.createElement("div");
    parent.className = "board";
    root.appendChild(parent);
    var kids = [];
    for (var c = 0; c < 32; c++) {
        var sq = document.createElement("div");
        sq.className = "sq";
        sq.id = "sq" + c;
        sq.setAttribute("data-x", "" + c);
        sq.classList.add("dark");
        sq.classList.toggle("lite");
        sq.style.backgroundColor = c % 2 ? "#eeeed2" : "#769656";
        sq.style.setProperty("grid-row", "" + (c % 8));
        sq.textContent = "piece" + c;
        parent.appendChild(sq);
        kids.push(sq);
    }
    // repeated wrapper property traffic
    for (var p = 0; p < 200; p++) {
        var s = kids[p % 32];
        out += (s.id === ("sq" + (p % 32))) ? 1 : 0;
        out += s.className.length + s.classList.length;
        s.setAttribute("data-p", "" + p);
        out += s.getAttribute("data-x").length;
        if (s.hasAttribute("data-x")) out += 1;
        s.style.left = "" + p + "px";
    }
    // insertBefore / removeChild churn
    var marker = kids[0];
    for (var r = 0; r < 40; r++) {
        var tmp = kids[31 - (r % 31)];
        parent.insertBefore(tmp, marker);
        parent.removeChild(tmp);
        parent.appendChild(tmp);
    }
    // query traffic
    for (var q = 0; q < 30; q++) {
        out += document.getElementsByTagName("div").length;
        out += parent.getElementsByClassName("sq").length;
        if (kids[q % 32].matches(".sq")) out += 1;
    }
    // -- events ---------------------------------------------------------------
    var clicks = 0;
    parent.addEventListener("click", function (e) { clicks++; out += e.clientX; });
    root.addEventListener("click", function () { clicks++; });
    for (var e2 = 0; e2 < 20; e2++) {
        var ev = new Event("click", { bubbles: true });
        ev.clientX = e2;
        kids[e2 % 32].dispatchEvent(ev);
    }
    // -- scheduler --------------------------------------------------------------
    var micro = 0;
    for (var u = 0; u < 30; u++) queueMicrotask(function () { micro++; });
    var ch = new MessageChannel();
    ch.port1.onmessage = function () { micro++; };
    for (var w = 0; w < 10; w++) ch.port2.postMessage("m" + w);
    setTimeout(function () { micro++; }, 0);
    requestAnimationFrame(function () { micro++; });
    window.__bench_out = out + micro;
    return out;
})()
"##;

fn main() {
    let iters: usize = std::env::args()
        .skip_while(|a| a != "--iters")
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let html = r#"<!doctype html><html><body><div id="root"></div></body></html>"#;
    let doc = rowser_html::parse(html);
    let mut dc = rowser_js::dom_bridge::DomContext::with_viewport(
        doc,
        "https://goonchess.com/",
        1920.0,
        1080.0,
    );

    // warmup: one iteration so prototypes/wrappers exist
    if let Err(e) = dc.eval(WORK) {
        eprintln!("warmup eval failed: {e}");
        std::process::exit(1);
    }
    dc.pump_bounded(std::time::Duration::from_secs(5));

    let mut best = f64::MAX;
    let mut worst = 0.0f64;
    let mut total = 0.0f64;
    for it in 0..iters {
        let t = Instant::now();
        match dc.eval(WORK) {
            Ok(_) => {}
            Err(e) => {
                eprintln!("iter {it} eval failed: {e}");
                std::process::exit(1);
            }
        }
        dc.pump_bounded(std::time::Duration::from_secs(5));
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        total += ms;
        best = best.min(ms);
        worst = worst.max(ms);
        if iters <= 30 {
            println!("iter {:>3}: {:8.2} ms", it, ms);
        }
    }
    println!(
        "bench2: {} iters  avg {:8.2} ms  best {:8.2} ms  worst {:8.2} ms",
        iters,
        total / iters as f64,
        best,
        worst
    );
}
