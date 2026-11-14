fn main() {
    let src = std::fs::read_to_string(std::env::args().nth(1).unwrap()).unwrap();
    let mut ctx = rowser_js::Context::new();
    match ctx.eval(&src) {
        Ok(v) => println!("OK: {:?}", ctx.interp.inspect(&v, 0)),
        Err(e) => println!("ERR: {}", e),
    }
}
