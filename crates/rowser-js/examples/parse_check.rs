//! Parse-only check: lex+parse a JS file, report first error with position.
//! Usage: cargo run -p rowser-js --example parse_check -- <file.js>

fn main() {
    let path = std::env::args().nth(1).expect("usage: parse_check <file.js>");
    let src = std::fs::read_to_string(&path).expect("read failed");
    let t = std::time::Instant::now();
    match rowser_js::parser::parse(&src) {
        Ok(stmts) => {
            println!(
                "PARSE OK: {} bytes, {} stmts, {:?}",
                src.len(),
                stmts.len(),
                t.elapsed()
            );
        }
        Err(e) => {
            println!("PARSE FAIL: {e}");
            // show context around the error line
            if e.line > 0 {
                for (n, l) in src.lines().enumerate() {
                    if (n as u32 + 1).abs_diff(e.line) <= 1 {
                        let marker = if n as u32 + 1 == e.line { ">>" } else { "  " };
                        let shown: String = l.chars().take(200).collect();
                        println!("{marker}{:6}| {shown}", n + 1);
                    }
                }
            }
            std::process::exit(1);
        }
    }
}
