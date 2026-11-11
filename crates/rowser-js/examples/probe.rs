fn main() {
    for s in [
        "var o=1; function f(){var o=Symbol.for('x');} f(); typeof o",
        "var o=1; (function(){var o=Symbol.for('x');})(); typeof o",
        "var o=1; var f=()=>{var o=Symbol.for('x');}; f(); typeof o",
        "var o=1; var f=(e)=>()=>e((e={exports:{}}).exports,e,e.exports); var l=f(function(e){var o=Symbol.for('x');}); l(); typeof o",
        "var o=1; function f(){o=Symbol.for('x');} f(); typeof o",
        "var o=1; function f(){for(o in {}){}} f(); typeof o",
        "var o=1; function f(){var g=function(){var o=Symbol.for('x');};g();} f(); typeof o",
        "var o=1; { var o=Symbol.for('x'); } typeof o",
    ] {
        let mut c = rowser_js::Context::new();
        match c.eval(s) {
            Ok(v) => println!("{:>95} => {}", s, c.interp.to_str(&v).unwrap_or_default()),
            Err(e)=>println!("{:>95} => ERR {e}", s),
        }
    }
}
