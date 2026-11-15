//! Eval check: run a JS file in a fresh Context (no DOM), report result/error.
//! Usage: cargo run -p rowser-js --example eval_check -- <file.js> [stack_mb]
fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: eval_check <file.js>");
    let src = std::fs::read_to_string(&path).expect("read failed");
    let h = std::thread::Builder::new()
        .stack_size(256 << 20)
        .spawn(move || {
            let mut ctx = rowser_js::Context::new();
            let secs: u64 = std::env::var("RJS_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10);
            if secs > 0 {
                ctx.interp.arm_deadline(std::time::Duration::from_secs(secs));
            }
            let t = std::time::Instant::now();
            match ctx.eval(&src) {
                Ok(v) => println!(
                    "EVAL OK in {:?}: {}",
                    t.elapsed(),
                    ctx.interp.inspect(&v, 0)
                ),
                Err(e) => println!("EVAL FAIL: {e}"),
            }
            let out = ctx.take_output();
            if !out.is_empty() {
                println!("console output ({}):", out.len());
                for l in out.iter().take(10) {
                    println!("  {l}");
                }
            }
        })
        .unwrap();
    h.join().unwrap();
}
