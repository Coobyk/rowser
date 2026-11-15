//! Micro-benchmarks for the tree-walking interpreter.
//! Usage: cargo run -p rowser-js --release --example bench
use std::time::Instant;

const CASES: &[(&str, &str)] = &[
    (
        "arith_loop",
        "let s = 0; for (let i = 0; i < 2000000; i++) { s += i; } s",
    ),
    (
        "member_read",
        "let o = {a: 1, b: 2, c: 3}; let s = 0; \
         for (let i = 0; i < 2000000; i++) { s += o.a + o.b + o.c; } s",
    ),
    (
        "member_write",
        "let o = {a: 0}; for (let i = 0; i < 1000000; i++) { o.a = i; } o.a",
    ),
    (
        "fn_calls",
        "function f(x) { return x * 2 + 1; } let s = 0; \
         for (let i = 0; i < 1000000; i++) { s += f(i); } s",
    ),
    (
        "method_calls",
        "let o = {v: 1, m(x) { return this.v + x; }}; let s = 0; \
         for (let i = 0; i < 500000; i++) { s += o.m(i); } s",
    ),
    (
        "arr_index",
        "let a = [1,2,3,4,5]; let s = 0; \
         for (let i = 0; i < 1000000; i++) { s += a[i % 5]; } s",
    ),
    (
        "string_add",
        "let s = ''; for (let i = 0; i < 20000; i++) { s += 'x'; } s.length",
    ),
    (
        "closures",
        "function mk(n) { return function() { return n + 1; }; } \
         let f = mk(41); let s = 0; \
         for (let i = 0; i < 500000; i++) { s += f(); } s",
    ),
    (
        "mixed",
        "function Node(v) { this.v = v; this.next = null; } \
         let head = null; \
         for (let i = 0; i < 200000; i++) { let n = new Node(i); n.next = head; head = n; } \
         let s = 0; let cur = head; \
         while (cur) { s += cur.v; cur = cur.next; } s",
    ),
];

fn main() {
    let mut handles = Vec::new();
    for (name, src) in CASES {
        let name = name.to_string();
        let src = src.to_string();
        handles.push(
            std::thread::Builder::new()
                .stack_size(256 << 20)
                .spawn(move || {
                    let mut ctx = rowser_js::Context::new();
                    // warmup + correctness run
                    let r = ctx.interp.run(&src);
                    let t = Instant::now();
                    let best = (0..3)
                        .map(|_| {
                            let mut ctx = rowser_js::Context::new();
                            let t0 = Instant::now();
                            let _ = ctx.interp.run(&src);
                            t0.elapsed()
                        })
                        .min()
                        .unwrap();
                    println!(
                        "{name:>14}: total {:?}  best-run {:?}  [{:?}]",
                        t.elapsed(),
                        best,
                        r.map(|_| ())
                    );
                })
                .unwrap(),
        );
    }
    for h in handles {
        h.join().unwrap();
    }
}
