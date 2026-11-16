fn main() {
    let src = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let mut ctx = rowser_js::Context::new();
    // wall-clock limit so runaway scripts (infinite loops, growing
    // iterators) error out instead of spinning — override via
    // RJS_TIMEOUT_SECS=0 to disable
    let secs: u64 = std::env::var("RJS_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    if secs > 0 {
        ctx.interp
            .arm_deadline(std::time::Duration::from_secs(secs));
    }
    match ctx.eval(&src) {
        Ok(v) => println!("OK: {:?}", ctx.interp.inspect(&v, 0)),
        Err(e) => println!("ERR: {}", e),
    }
    for line in ctx.output() {
        println!("| {line}");
    }
}
