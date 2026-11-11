//! rowser-js — a from-scratch JavaScript interpreter (lexer → parser →
//! tree-walking evaluator) built on std only, for the rowser browser.
//!
//! ```
//! let mut ctx = rowser_js::Context::new();
//! let v = ctx.eval("1 + 2").unwrap();
//! ```

pub mod ast;
pub mod builtins;
pub mod dom_bridge;
pub mod interp;
pub mod lexer;
pub mod parser;
pub mod regexp;

use interp::{Interp, Value};

/// A lexer/parser/runtime error with position information.
#[derive(Debug, Clone)]
pub struct Error {
    pub msg: String,
    pub line: u32,
    pub col: u32,
}

impl Error {
    pub fn new(msg: impl Into<String>, line: u32, col: u32) -> Error {
        Error {
            msg: msg.into(),
            line,
            col,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line > 0 {
            write!(f, "{} at {}:{}", self.msg, self.line, self.col)
        } else {
            write!(f, "{}", self.msg)
        }
    }
}

impl std::error::Error for Error {}

/// A persistent JS context: global environment + builtins + console output.
/// DOM bindings will later register host objects on `ctx.interp`.
pub struct Context {
    pub interp: Interp,
}

impl Context {
    pub fn new() -> Context {
        let mut interp = Interp::new();
        builtins::install(&mut interp);
        Context { interp }
    }

    /// Evaluate a script in the persistent global scope.
    pub fn eval(&mut self, src: &str) -> Result<Value, Error> {
        self.interp.run(src)
    }

    /// Collected console.log/warn/error output.
    pub fn output(&self) -> &[String] {
        &self.interp.output
    }

    pub fn take_output(&mut self) -> Vec<String> {
        std::mem::take(&mut self.interp.output)
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

/// One-shot evaluate: fresh context, run source, return final value.
pub fn eval(src: &str) -> Result<Value, Error> {
    Context::new().eval(src)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> Value {
        match eval(src) {
            Ok(v) => v,
            Err(e) => panic!("eval failed for `{src}`: {e}"),
        }
    }

    fn num(src: &str) -> f64 {
        match run(src) {
            Value::Num(n) => n,
            _ => panic!("expected number for `{src}`"),
        }
    }

    fn s(src: &str) -> String {
        match run(src) {
            Value::Str(s) => s.to_string(),
            _ => panic!("expected string for `{src}`"),
        }
    }

    fn b(src: &str) -> bool {
        match run(src) {
            Value::Bool(v) => v,
            _ => panic!("expected bool for `{src}`"),
        }
    }

    // --- arithmetic & coercion ---
    #[test]
    fn arithmetic() {
        assert_eq!(num("1 + 2 * 3"), 7.0);
        assert_eq!(num("(1 + 2) * 3"), 9.0);
        assert_eq!(num("10 / 4"), 2.5);
        assert_eq!(num("10 % 3"), 1.0);
        assert_eq!(num("2 ** 10"), 1024.0);
        assert_eq!(num("-5 + -3"), -8.0);
        assert_eq!(num("0x10 + 0o17 + 0b11"), 16.0 + 15.0 + 3.0);
        assert_eq!(num("1e3 + 2e-1"), 1000.2);
    }

    #[test]
    fn string_number_coercion() {
        assert_eq!(s("1 + 'a'"), "1a");
        assert_eq!(s("'a' + 1"), "a1");
        assert_eq!(s("'a' + true"), "atrue");
        assert_eq!(num("'5' - 2"), 3.0);
        assert_eq!(num("'5' * '2'"), 10.0);
        assert_eq!(num("+('42')"), 42.0);
        assert_eq!(s("1 + 2 + 'x'"), "3x");
        assert_eq!(s("'x' + 1 + 2"), "x12");
    }

    #[test]
    fn equality() {
        assert!(b("1 == '1'"));
        assert!(!b("1 === '1'"));
        assert!(b("null == undefined"));
        assert!(!b("null === undefined"));
        assert!(b("NaN !== NaN"));
        assert!(b("'abc' === 'abc'"));
        assert!(b("true == 1"));
        assert!(b("0 == false"));
        assert!(b("'' == false"));
        assert!(!b("[] === []"));
    }

    #[test]
    fn comparisons() {
        assert!(b("1 < 2"));
        assert!(b("'b' > 'a'"));
        assert!(b("'10' < '9'")); // string compare
        assert!(b("10 > 9"));
        assert!(!b("NaN < 1"));
        assert!(!b("NaN > 1"));
    }

    #[test]
    fn truthiness() {
        assert!(b("!0"));
        assert!(b("!''"));
        assert!(b("!!1"));
        assert!(b("!!'x'"));
        assert!(b("!![]"));
        assert!(b("!!{}"));
        assert!(b("!null"));
        assert!(b("!undefined"));
        assert!(b("!NaN"));
    }

    #[test]
    fn logical_and_ternary() {
        assert_eq!(num("0 || 5"), 5.0);
        assert_eq!(num("1 && 5"), 5.0);
        assert_eq!(num("0 && 5"), 0.0);
        assert_eq!(num("null ?? 7"), 7.0);
        assert_eq!(num("0 ?? 7"), 0.0);
        assert_eq!(num("undefined ?? 7"), 7.0);
        assert_eq!(num("1 > 0 ? 'yes'.length : 0"), 3.0);
        assert_eq!(num("false ? 1 : 2"), 2.0);
    }

    #[test]
    fn unary_ops() {
        assert_eq!(s("typeof 1"), "number");
        assert_eq!(s("typeof 'a'"), "string");
        assert_eq!(s("typeof true"), "boolean");
        assert_eq!(s("typeof undefined"), "undefined");
        assert_eq!(s("typeof null"), "object");
        assert_eq!(s("typeof {}"), "object");
        assert_eq!(s("typeof (() => 1)"), "function");
        assert_eq!(s("typeof nonexistent_ident"), "undefined");
        assert_eq!(num("void 0 ?? 9"), 9.0);
        assert_eq!(num("~5"), -6.0);
    }

    #[test]
    fn bitwise() {
        assert_eq!(num("5 & 3"), 1.0);
        assert_eq!(num("5 | 3"), 7.0);
        assert_eq!(num("5 ^ 3"), 6.0);
        assert_eq!(num("1 << 4"), 16.0);
        assert_eq!(num("256 >> 4"), 16.0);
        assert_eq!(num("-1 >>> 28"), 15.0);
    }

    // --- variables & scope ---
    #[test]
    fn var_let_const() {
        assert_eq!(num("var x = 5; x"), 5.0);
        assert_eq!(num("let y = 6; y"), 6.0);
        assert_eq!(num("const z = 7; z"), 7.0);
        assert_eq!(num("var a = 1, b = 2; a + b"), 3.0);
        assert_eq!(num("x2 = 9; x2"), 9.0); // sloppy global
    }

    #[test]
    fn var_hoisting() {
        assert_eq!(num("f(); function f() { return 42 }"), 42.0);
        assert!(matches!(run("v; var v = 3; v"), Value::Num(3.0)));
        assert!(eval("u").is_err()); // unbound
        assert!(matches!(run("w; var w = 1"), Value::Und));
    }

    #[test]
    fn block_scope() {
        assert_eq!(num("{ let x = 1; } var y = 2; y"), 2.0);
        assert!(matches!(eval("{ let q = 1 } q"), Err(_)));
        assert_eq!(num("let a = 1; { let a = 2; } a"), 1.0);
    }

    #[test]
    fn closures() {
        assert_eq!(
            num("function counter() { let n = 0; return () => ++n; } let c = counter(); c(); c(); c()"),
            3.0
        );
        assert_eq!(
            num("let fns = []; for (var i = 0; i < 3; i++) { fns.push(() => i); } fns[0]() + fns[1]() + fns[2]()"),
            9.0
        );
        assert_eq!(
            num("function outer() { let x = 1; function inner() { return x + 1; } return inner(); } outer()"),
            2.0
        );
    }

    #[test]
    fn arrow_functions() {
        assert_eq!(num("let add = (a, b) => a + b; add(2, 3)"), 5.0);
        assert_eq!(num("let inc = n => n + 1; inc(4)"), 5.0);
        assert_eq!(num("let f = () => { return 10; }; f()"), 10.0);
        assert_eq!(num("[1,2,3].map(x => x * 2)[2]"), 6.0);
        // lexical this
        assert_eq!(
            num("let o = { n: 5, f: function() { return (() => this.n)(); } }; o.f()"),
            5.0
        );
    }

    #[test]
    fn default_rest_params() {
        assert_eq!(num("function f(a, b = 10) { return a + b; } f(1)"), 11.0);
        assert_eq!(num("function g(...rest) { return rest.length; } g(1,2,3)"), 3.0);
        assert_eq!(num("function h(a, ...rest) { return rest.join(','); } h(1,2,3).length"), 3.0);
        assert_eq!(num("function k(a = 1, b = a + 1) { return b; } k()"), 2.0);
    }

    #[test]
    fn arguments_object() {
        assert_eq!(num("function f() { return arguments.length; } f(1,2,3)"), 3.0);
        assert_eq!(s("function f() { return arguments[1]; } f('a','b')"), "b");
        assert_eq!(s("function f() { return arguments[0] + arguments[1]; } f('a','b')"), "ab");
    }

    #[test]
    fn iife_and_module_pattern() {
        assert_eq!(num("(function() { return 7; })()"), 7.0);
        assert_eq!(num("(() => 8)()"), 8.0);
        // classic module pattern like bundled JS
        let src = r#"
            var mod = (function(exports) {
                'use strict';
                let state = 0;
                exports.bump = () => ++state;
                exports.get = () => state;
                return exports;
            })({});
            mod.bump(); mod.bump(); mod.bump();
            mod.get()
        "#;
        assert_eq!(num(src), 3.0);
        // webpack-style IIFE with params
        let src2 = r#"
            var out;
            ((e, t) => {
                e.val = t + 1;
            })(this ?? {}, 41);
            out = (typeof this === 'undefined') ? 1 : 2;
            out
        "#;
        let v = eval(src2);
        assert!(v.is_ok());
    }

    // --- objects ---
    #[test]
    fn object_literals() {
        assert_eq!(num("let o = {a: 1, 'b': 2, 3: 4}; o.a + o['b'] + o[3]"), 7.0);
        assert_eq!(num("let k = 'x'; let o = {[k + 'y']: 5}; o.xy"), 5.0);
        assert_eq!(num("let v = 9; let o = {v}; o.v"), 9.0);
        assert_eq!(num("let o = {m() { return 3; }}; o.m()"), 3.0);
        assert_eq!(num("let o = {a:1, b:2}; Object.keys(o).length"), 2.0);
    }

    #[test]
    fn object_mutation_and_delete() {
        assert_eq!(num("let o = {}; o.x = 5; o.x += 2; o.x"), 7.0);
        assert_eq!(num("let o = {a: 1, b: 2}; delete o.a; 'a' in o ? 1 : 0"), 0.0);
        assert!(b("'x' in {x: 1}"));
        assert!(b("let o = {a:{b:{c:4}}}; o.a.b.c === 4"));
        assert!(b("let o = {a:1}; o?.a === 1"));
        assert_eq!(num("let o = null; o?.x === undefined ? 1 : 0"), 1.0);
        assert_eq!(num("let o = {a:{}}; o?.a?.b === undefined ? 5 : 6"), 5.0);
    }

    #[test]
    fn getters_setters() {
        assert_eq!(
            num("let o = { _v: 1, get v() { return this._v * 10; }, set v(x) { this._v = x; } }; o.v = 4; o.v"),
            40.0
        );
    }

    #[test]
    fn prototypes_and_this() {
        assert_eq!(
            num("function P(n) { this.n = n; } P.prototype.get = function() { return this.n; }; let p = new P(42); p.get()"),
            42.0
        );
        assert!(b("function F(){}; let f = new F(); f instanceof F"));
        assert!(b("let a = [1,2]; a instanceof Array"));
        assert!(b("let o = {}; o instanceof Object"));
        // proto chain method lookup
        assert_eq!(
            num("function A(){} A.prototype.hello = function(){ return 'hi'.length }; let a = new A(); a.hello()"),
            2.0
        );
    }

    #[test]
    fn object_statics() {
        assert_eq!(num("Object.keys({a:1,b:2,c:3}).length"), 3.0);
        assert_eq!(num("Object.values({a:1,b:2}).reduce((s,v)=>s+v)"), 3.0);
        assert_eq!(num("Object.assign({a:1},{b:2},{c:3}).b"), 2.0);
        assert_eq!(num("Object.entries({x:5})[0][0].length + Object.entries({x:5})[0][1]"), 6.0);
        assert!(b("let p = {z:9}; let o = Object.create(p); o.z === 9"));
        assert_eq!(
            num("let o = {}; Object.defineProperty(o, 'x', {value: 42, writable: false}); o.x = 1; o.x"),
            42.0
        );
        assert_eq!(
            num("let o = {}; Object.defineProperty(o, 'g', {get: () => 11}); o.g"),
            11.0
        );
        assert!(b("Object.getPrototypeOf([]) === Array.prototype"));
        assert!(b("let o = {}; Object.setPrototypeOf(o, {y:3}); o.y === 3"));
        assert!(b("Object.freeze({a:1}) instanceof Object"));
        assert_eq!(num("Object.fromEntries([['a',1],['b',2]]).b"), 2.0);
        assert!(b("({a:1}).hasOwnProperty('a')"));
        assert!(!b("({}).hasOwnProperty('a')"));
    }

    #[test]
    fn destructuring() {
        assert_eq!(num("let [a, b] = [1, 2]; a + b"), 3.0);
        assert_eq!(num("let {x, y} = {x: 1, y: 2}; x + y"), 3.0);
        assert_eq!(num("let {x: p, y: q} = {x: 10, y: 20}; p + q"), 30.0);
        assert_eq!(num("let [a = 5, b = 6] = [1]; a + b"), 7.0);
        assert_eq!(num("let {a = 1} = {}; a"), 1.0);
        assert_eq!(num("let [x, ...rest] = [1,2,3,4]; rest.length"), 3.0);
        assert_eq!(num("let {a, ...o} = {a:1,b:2,c:3}; Object.keys(o).length"), 2.0);
        assert_eq!(num("var a, b; [a, b] = [3, 4]; a * b"), 12.0);
        assert_eq!(num("let {n: {m}} = {n: {m: 8}}; m"), 8.0);
        assert_eq!(num("function f({a, b}) { return a - b; } f({a: 10, b: 3})"), 7.0);
    }

    #[test]
    fn spread() {
        assert_eq!(num("let a = [1,2]; let b = [...a, 3]; b.length"), 3.0);
        assert_eq!(num("function s(...n){ return n.reduce((a,b)=>a+b,0); } s(...[1,2,3])"), 6.0);
        assert_eq!(num("let o = {...{a:1}, b:2}; o.a + o.b"), 3.0);
        assert_eq!(num("Math.max(...[3, 9, 2])"), 9.0);
    }

    // --- control flow ---
    #[test]
    fn control_flow() {
        assert_eq!(num("let x = 0; for (let i = 0; i < 10; i++) x += i; x"), 45.0);
        assert_eq!(num("let x = 0; let i = 0; while (i < 5) { x += i; i++; } x"), 10.0);
        assert_eq!(num("let i = 0; do { i++; } while (i < 3); i"), 3.0);
        assert_eq!(num("let s = ''; for (let k of [1,2,3]) s += k; s.length"), 3.0);
        assert_eq!(s("let s = ''; for (let c of 'abc') s += c + '.'; s"), "a.b.c.");
        assert_eq!(num("let n = 0; for (let k in {a:1,b:2,c:3}) n++; n"), 3.0);
        assert_eq!(num("let x = 0; for (let i = 0; i < 10; i++) { if (i == 5) break; x++; } x"), 5.0);
        assert_eq!(num("let x = 0; for (let i = 0; i < 10; i++) { if (i % 2) continue; x++; } x"), 5.0);
    }

    #[test]
    fn labels() {
        assert_eq!(
            num("let n = 0; outer: for (let i = 0; i < 3; i++) { for (let j = 0; j < 3; j++) { if (j > i) continue outer; n++; } } n"),
            6.0
        );
        assert_eq!(
            num("let n = 0; lbl: { n = 5; break lbl; n = 9; } n"),
            5.0
        );
    }

    #[test]
    fn switch_stmt() {
        assert_eq!(
            num("function f(x) { switch (x) { case 1: return 'a'.length; case 2: return 20; default: return 99; } } f(2)"),
            20.0
        );
        assert_eq!(
            num("function f(x) { switch (x) { case 1: return 1; default: return 99; } } f(5)"),
            99.0
        );
        // fallthrough
        assert_eq!(
            num("let r = 0; switch (2) { case 1: r += 1; case 2: r += 2; case 3: r += 3; break; case 4: r += 4; } r"),
            5.0
        );
    }

    #[test]
    fn try_catch_finally() {
        assert_eq!(
            num("let r = 0; try { throw new Error('x'); } catch (e) { r = 1; } r"),
            1.0
        );
        assert_eq!(
            num("let r = ''; try { r += 't'; throw 1; } catch (e) { r += 'c'; } finally { r += 'f'; } r.length"),
            3.0
        );
        assert_eq!(
            s("try { throw new TypeError('bad'); } catch (e) { e.name + ':' + e.message }"),
            "TypeError:bad"
        );
        assert!(b("let ok = false; try { null.x; } catch (e) { ok = e instanceof TypeError; } ok"));
        assert_eq!(
            num("try { JSON.parse('{bad'); } catch (e) { 42 }"),
            42.0
        );
    }

    // --- classes ---
    #[test]
    fn classes() {
        assert_eq!(
            num("class A { constructor(x) { this.x = x; } get() { return this.x; } } let a = new A(5); a.get()"),
            5.0
        );
        assert!(b("class A {} let a = new A(); a instanceof A"));
        assert_eq!(
            num("class A { static make() { return 3; } } A.make()"),
            3.0
        );
        assert_eq!(
            num("class A { get v() { return 7; } } new A().v"),
            7.0
        );
        assert_eq!(
            s("class A { set v(x) { this._v = 'set:' + x; } get v() { return this._v; } } let a = new A(); a.v = 'k'; a.v"),
            "set:k"
        );
    }

    #[test]
    fn class_extends() {
        assert_eq!(
            num(r#"
                class A { constructor() { this.n = 1; } hi() { return 'A'.length + this.n; } }
                class B extends A { constructor() { super(); this.n = 5; } }
                new B().hi()
            "#),
            6.0
        );
        assert_eq!(
            num(r#"
                class A { greet() { return 10; } }
                class B extends A { greet() { return super.greet() + 1; } }
                new B().greet()
            "#),
            11.0
        );
        assert!(b("class A {} class B extends A {} new B() instanceof A"));
        assert!(b("class A {} class B extends A {} new B() instanceof B"));
        assert_eq!(
            num("class A { static s() { return 4; } } class B extends A {} B.s()"),
            4.0
        );
        // default derived ctor forwards args
        assert_eq!(
            num("class A { constructor(x) { this.x = x; } } class B extends A {} new B(9).x"),
            9.0
        );
    }

    // --- arrays ---
    #[test]
    fn array_methods() {
        assert_eq!(num("let a = [1,2,3]; a.push(4); a.length"), 4.0);
        assert_eq!(num("let a = [1,2,3]; a.pop()"), 3.0);
        assert_eq!(num("let a = [1,2,3]; a.shift()"), 1.0);
        assert_eq!(num("let a = [1,2]; a.unshift(0); a[0]"), 0.0);
        assert_eq!(num("let a = [1,2,3,4]; a.slice(1,3).join('-').length"), 3.0);
        assert_eq!(s("let a = [1,2,3,4]; a.splice(1,2); a.join(',')"), "1,4");
        assert_eq!(num("let a = [1,2,3,4]; a.splice(1,2)[0]"), 2.0);
        assert_eq!(s("[1,2,3].join('-')"), "1-2-3");
        assert_eq!(num("[1,2,3].indexOf(2)"), 1.0);
        assert_eq!(num("[1,2,3].indexOf(9)"), -1.0);
        assert!(b("[1,2,3].includes(2)"));
        assert!(!b("[1,2,3].includes(9)"));
        assert_eq!(num("[1,2,3].concat([4,5]).length"), 5.0);
        assert_eq!(num("[3,1,2].sort()[0]"), 1.0);
        assert_eq!(num("[3,1,2].sort((a,b) => b - a)[0]"), 3.0);
        assert_eq!(num("[1,2,3].reverse()[0]"), 3.0);
        assert_eq!(num("[[1,2],[3,[4]]].flat().length"), 4.0);
        assert_eq!(num("[[1,[2]]].flat(2)[1]"), 2.0);
        assert_eq!(num("[1,2,3].fill(0).join('').length"), 3.0);
        assert_eq!(num("[1,2,3].at(-1)"), 3.0);
    }

    #[test]
    fn array_functional() {
        assert_eq!(num("[1,2,3,4].filter(x => x % 2 == 0).length"), 2.0);
        assert_eq!(s("[1,2,3].map(x => x * 2).join(',')"), "2,4,6");
        assert_eq!(num("[1,2,3,4].reduce((s, v) => s + v, 0)"), 10.0);
        assert_eq!(num("[1,2,3,4].reduce((s, v) => s + v)"), 10.0);
        assert!(b("[1,2,3].some(x => x > 2)"));
        assert!(!b("[1,2,3].every(x => x > 2)"));
        assert_eq!(num("[1,2,3].find(x => x > 1)"), 2.0);
        assert_eq!(num("[1,2,3].findIndex(x => x > 1)"), 1.0);
        assert_eq!(num("let s = 0; [1,2,3].forEach(x => s += x); s"), 6.0);
        // the classic pipeline
        assert_eq!(
            num("[1,2,3,4,5,6].filter(x => x % 2).map(x => x * x).reduce((a,b)=>a+b,0)"),
            35.0
        );
    }

    #[test]
    fn array_iterators() {
        assert_eq!(
            num("let s = 0; for (let v of [10,20,30].values()) s += v; s"),
            60.0
        );
        assert_eq!(
            num("let n = 0; for (let e of [1,2].entries()) { n += e[0] + e[1]; } n"),
            4.0
        );
        assert_eq!(
            num("let n = 0; for (let [k, v] of [[1,2],[3,4]]) { n += k * v; } n"),
            14.0
        );
        assert_eq!(num("Array.isArray([1]) ? 1 : 0"), 1.0);
        assert_eq!(num("Array.isArray('x') ? 1 : 0"), 0.0);
        assert_eq!(num("Array.from([1,2,3]).length"), 3.0);
        assert_eq!(num("Array.from('abc').length"), 3.0);
        assert_eq!(num("Array.of(1,2,3).length"), 3.0);
        assert_eq!(num("Array(3).length"), 3.0);
    }

    // --- strings ---
    #[test]
    fn string_methods() {
        assert_eq!(s("'hello'.charAt(1)"), "e");
        assert_eq!(num("'hello'.charCodeAt(0)"), 104.0);
        assert_eq!(s("'hello'.slice(1, 3)"), "el");
        assert_eq!(s("'hello'.slice(-2)"), "lo");
        assert_eq!(s("'hello'.substring(1, 3)"), "el");
        assert_eq!(num("'hello'.indexOf('l')"), 2.0);
        assert_eq!(num("'hello'.lastIndexOf('l')"), 3.0);
        assert_eq!(num("'a,b,c'.split(',').length"), 3.0);
        assert_eq!(s("'  hi  '.trim()"), "hi");
        assert_eq!(s("'aaa'.replace('a', 'b')"), "baa");
        assert_eq!(s("'aaa'.replaceAll('a', 'b')"), "bbb");
        assert_eq!(s("'5'.padStart(3, '0')"), "005");
        assert_eq!(s("'5'.padEnd(3, '0')"), "500");
        assert!(b("'hello'.startsWith('he')"));
        assert!(b("'hello'.endsWith('lo')"));
        assert!(b("'hello'.includes('ell')"));
        assert_eq!(s("'ab'.repeat(3)"), "ababab");
        assert_eq!(s("'ABC'.toLowerCase()"), "abc");
        assert_eq!(s("'abc'.toUpperCase()"), "ABC");
        assert_eq!(num("'hello'.length"), 5.0);
        assert_eq!(s("'hé'.length > 0 ? 'yes' : 'no'"), "yes");
        assert_eq!(s("String(123)"), "123");
        assert_eq!(s("String.fromCharCode(72, 105)"), "Hi");
        assert_eq!(s("'x' + null"), "xnull");
    }

    #[test]
    fn template_literals() {
        assert_eq!(s("let x = 5; `value: ${x}`"), "value: 5");
        assert_eq!(s("`a${1 + 1}b${'c'.toUpperCase()}`"), "a2bC");
        assert_eq!(s("`` + 1"), "1");
        assert_eq!(s("`nested ${`inner ${2}`} done`"), "nested inner 2 done");
    }

    // --- numbers ---
    #[test]
    fn number_methods() {
        assert_eq!(s("(3.14159).toFixed(2)"), "3.14");
        assert_eq!(s("(255).toString(16)"), "ff");
        assert_eq!(s("(8).toString(2)"), "1000");
        assert_eq!(num("parseInt('42px')"), 42.0);
        assert_eq!(num("parseInt('0x1F')"), 31.0);
        assert_eq!(num("parseInt('11', 2)"), 3.0);
        assert!((num("parseFloat('3.14xyz')") - 3.14).abs() < 0.001);
        assert!(b("isNaN('abc')"));
        assert!(!b("isNaN('12')"));
        assert!(b("Number.isNaN(NaN)"));
        assert!(!b("Number.isNaN('abc')"));
        assert!(b("isFinite('12')"));
        assert!(!b("isFinite('abc')"));
        assert_eq!(num("Number('42')"), 42.0);
        assert!(b("Number.isInteger(5)"));
        assert!(!b("Number.isInteger(5.5)"));
    }

    #[test]
    fn math() {
        assert_eq!(num("Math.floor(3.7)"), 3.0);
        assert_eq!(num("Math.ceil(3.2)"), 4.0);
        assert_eq!(num("Math.round(3.5)"), 4.0);
        assert_eq!(num("Math.abs(-5)"), 5.0);
        assert_eq!(num("Math.min(3, 1, 2)"), 1.0);
        assert_eq!(num("Math.max(3, 1, 2)"), 3.0);
        assert_eq!(num("Math.pow(2, 8)"), 256.0);
        assert_eq!(num("Math.sqrt(16)"), 4.0);
        let r = num("Math.random()");
        assert!((0.0..1.0).contains(&r));
        assert_eq!(num("Math.PI > 3 ? 1 : 0"), 1.0);
        assert_eq!(num("Math.sign(-10)"), -1.0);
    }

    // --- JSON ---
    #[test]
    fn json_round_trip() {
        assert_eq!(s("JSON.stringify({a: 1, b: 'x', c: [1,2], d: null})"), r#"{"a":1,"b":"x","c":[1,2],"d":null}"#);
        assert_eq!(s("JSON.stringify([1, 'a', true, null])"), r#"[1,"a",true,null]"#);
        assert_eq!(s("JSON.stringify('hi')"), r#""hi""#);
        assert_eq!(num("JSON.parse('{\"a\": 5}').a"), 5.0);
        assert_eq!(num("JSON.parse('[1,2,3]')[1]"), 2.0);
        assert!(b("JSON.parse('true') === true"));
        assert!(matches!(run("JSON.parse('null')"), Value::Null));
        assert_eq!(s("let o = {n: 'x', arr: [{k: 1}]}; JSON.parse(JSON.stringify(o)).arr[0].k === 1 ? 'ok' : 'bad'"), "ok");
        assert_eq!(num("Object.keys(JSON.parse('{\"x\":1,\"y\":2}')).length"), 2.0);
    }

    // --- promises (synchronous stub) ---
    #[test]
    fn promise_stub() {
        assert_eq!(
            num("let r = 0; new Promise((res) => res(5)).then(v => r = v); r"),
            5.0
        );
        assert_eq!(
            num("let r = 0; new Promise((res, rej) => rej('e')).catch(v => r = 9); r"),
            9.0
        );
        assert_eq!(
            num("let r = 0; Promise.resolve(3).then(v => v * 2).then(v => r = v); r"),
            6.0
        );
        assert_eq!(
            num("let r = 0; Promise.all([Promise.resolve(1), Promise.resolve(2)]).then(a => r = a[0] + a[1]); r"),
            3.0
        );
    }

    // --- misc ---
    #[test]
    fn misc() {
        assert_eq!(num("1, 2, 3"), 3.0); // sequence
        assert_eq!(num("let i = 0; i++; i++; ++i; i"), 3.0);
        assert_eq!(num("let i = 5; i-- + i"), 9.0);
        assert_eq!(s("Symbol('x').toString()"), "Symbol(x)");
        assert!(b("typeof Symbol() === 'symbol'"));
        assert_eq!(num("Date.now() > 0 ? 1 : 0"), 1.0);
        // use strict is a no-op
        assert_eq!(num("'use strict'; 40 + 2"), 42.0);
        assert_eq!(
            num("let m = new Map(); m.set('a', 1); m.set('b', 2); m.get('a') + m.size"),
            3.0
        );
        assert_eq!(
            num("let s = new Set([1,1,2]); s.size"),
            2.0
        );
        // conditional member on null
        assert!(matches!(run("let o = null; o?.x"), Value::Und));
        // in/instanceof/typeof combos
        assert!(b("typeof Array.isArray === 'function'"));
        // computed member + call chains
        assert_eq!(num("let o = {f: {g: () => 12}}; o.f['g']()"), 12.0);
        // recursion
        assert_eq!(num("function fib(n) { return n < 2 ? n : fib(n-1) + fib(n-2); } fib(15)"), 610.0);
        // eval of a whole bundled-style program
        let prog = r#"
            'use strict';
            var app = (function() {
                const handlers = [];
                const state = { count: 0, items: [] };
                function dispatch(action) {
                    switch (action.type) {
                        case 'inc': state.count++; break;
                        case 'add': state.items.push(action.item); break;
                    }
                    handlers.forEach(h => h(state));
                }
                return {
                    dispatch,
                    subscribe: fn => handlers.push(fn),
                    getState: () => state,
                };
            })();
            let seen = 0;
            app.subscribe(s => seen = s.count);
            app.dispatch({type: 'inc'});
            app.dispatch({type: 'inc'});
            app.dispatch({type: 'add', item: 'x'});
            app.getState().items.length * 100 + seen
        "#;
        assert_eq!(num(prog), 102.0);
    }

    #[test]
    fn console_output() {
        let mut ctx = Context::new();
        ctx.eval("console.log('hello', 42, {a:1}, [1,2]); console.warn('w')").unwrap();
        let out = ctx.output();
        assert_eq!(out.len(), 2);
        assert!(out[0].contains("hello"));
        assert!(out[0].contains("42"));
    }

    #[test]
    fn persistent_context() {
        let mut ctx = Context::new();
        ctx.eval("var g = 10; function f() { return g * 2; }").unwrap();
        assert_eq!(num_ctx(&mut ctx, "f()"), 20.0);
        ctx.eval("g = 50").unwrap();
        assert_eq!(num_ctx(&mut ctx, "f()"), 100.0);
    }

    fn num_ctx(ctx: &mut Context, src: &str) -> f64 {
        match ctx.eval(src).unwrap() {
            Value::Num(n) => n,
            _ => panic!("expected number"),
        }
    }

    // ============================ regex ============================

    #[test]
    fn regex_literal_lexing_contexts() {
        // after `=` — regex
        assert!(b("var r = /a+/; r.test('baa')"));
        // at statement start / after `;` — regex
        assert!(b("var x = 0; /a/.test('a')"));
        // after `(` — regex
        assert!(b("var x = 'a'; if (/a/.test(x)) { x = 1; } x === 1"));
        // after `return` — regex
        assert!(b("function f() { return /\\d/.test('7'); } f()"));
        // after `,` — regex
        assert!(b("var a = 1, r = /x/; r.test('x')"));
        // after `:` and `?` — regex
        assert!(b("var o = {re: /z/}; o.re.test('z')"));
        assert!(b("var v = true ? /y/.test('y') : false; v"));
        // after operator — regex
        assert!(b("var t = !/q/.test('a'); t"));
        assert!(b("var t = /a/.test('a') && /b/.test('b'); t"));
        // after `=>` — regex
        assert!(b("var f = () => /w/.test('w'); f()"));
        // division still works in operand contexts
        assert_eq!(num("var a = 10; a / 2"), 5.0);
        assert_eq!(num("var a = 20, b = 4; a / b / 2"), 2.5);
        assert_eq!(num("var x = 8, y = 2, z = x / y; z / 2"), 2.0);
        assert_eq!(num("(10 + 2) / 3"), 4.0);
        assert_eq!(num("var r = /x/; 8 / 4"), 2.0);
        // `/=` division-assign vs regex starting with `=`
        assert_eq!(num("var a = 10; a /= 2; a"), 5.0);
        assert!(b("var r = /=x+/; r.test('=xx')"));
        // regex after `)` of a call stays division
        assert_eq!(num("function t(){return 9} var q = t() / 3; q"), 3.0);
    }

    #[test]
    fn regex_basics() {
        assert!(b("/abc/.test('xxabcxx')"));
        assert!(!b("/abc/.test('ab')"));
        assert!(b("/a.c/.test('axc')"));
        assert!(!b("/a.c/.test('a\\nc')")); // . excludes newline
        assert!(b("/a.c/s.test('a\\nc')")); // dotAll
        assert!(b("/^[a-z]+$/.test('hello')"));
        assert!(!b("/^[a-z]+$/.test('hello9')"));
        assert!(b("/^b/m.test('a\\nb')"));
        assert!(b("/d$/m.test('c\\nd')"));
    }

    #[test]
    fn regex_classes_and_escapes() {
        assert!(b("/[aeiou]/.test('bad')"));
        assert!(!b("/[aeiou]/.test('bcdfg')"));
        assert!(b("/[^aeiou]/.test('bcd')"));
        assert!(b("/[a-z]+/.test('HELLOabc')"));
        assert!(b("/\\d{3}-\\d{4}/.test('call 555-1234')"));
        assert!(b("/\\w+/.test('  hi_9 ')"));
        assert!(b("/\\s/.test('a b')"));
        assert!(b("/\\S+/.test('  x  ')"));
        assert!(b("/[=:]/.test('a=b')"));
        assert!(b("/\\/+/.test('a//b')"));
        assert!(b("/\\bword\\b/.test('a word here')"));
        assert!(!b("/\\bword\\b/.test('swordfish')"));
        assert!(b("/\\x41\\u0042/.test('AB')"));
    }

    #[test]
    fn regex_quantifiers() {
        assert!(b("/ab*c/.test('abbbbc')"));
        assert!(b("/ab+c/.test('abc')"));
        assert!(!b("/ab+c/.test('ac')"));
        assert!(b("/ab?c/.test('ac')"));
        assert!(b("/a{2,3}b/.test('aab')"));
        assert!(!b("/a{2,3}b/.test('ab')"));
        assert!(!b("/^a{2,3}b$/.test('aaaab')"));
        assert!(b("/a{2,3}b/.test('aaaab')")); // 'aaab' contains a{3}+b
        assert!(b("/a{2,}b/.test('aaaab')"));
        assert!(b("/a{2}b/.test('aab')"));
        // lazy
        assert_eq!(s("'<ab>'.match(/<(.+?)>/)[1]"), "ab");
        assert_eq!(s("'<<a>>'.match(/<(.+?)>/)[1]"), "<a");
        assert_eq!(s("'aaa'.match(/a+?/)[0]"), "a");
        assert_eq!(s("'aaa'.match(/a+?a/)[0]"), "aa");
        assert_eq!(s("'<x><y>'.match(/<.*>/)[0]"), "<x><y>");
        assert_eq!(s("'<x><y>'.match(/<.*?>/)[0]"), "<x>");
        // alternation + groups
        assert!(b("/cat|dog/.test('i have a dog')"));
        assert_eq!(s("'abab'.match(/(ab|cd)+/)[0]"), "abab");
        assert!(b("/(ab)+/.test('ababab')"));
    }

    #[test]
    fn regex_groups_and_backrefs() {
        assert_eq!(s("var m = /(\\d+)-(\\d+)/.exec('12-34'); m[1] + '/' + m[2]"), "12/34");
        assert_eq!(num("var m = /b(c)/.exec('abc'); m.index"), 1.0);
        assert_eq!(s("var m = /b(c)/.exec('abc'); m.input"), "abc");
        assert!(b("/(a|b)\\1/.test('aa')"));
        assert!(!b("/(a|b)\\1/.test('ab')"));
        assert!(b("/(\\w+)\\s+\\1/.test('hello hello')"));
        // non-capture group isn't counted
        assert_eq!(s("var m = /(?:a)(b)/.exec('ab'); m.length + ':' + m[1]"), "2:b");
        // named groups
        assert_eq!(
            s("var m = /(?<yr>\\d{4})-(?<mo>\\d{2})/.exec('2024-05'); m.groups.yr + '/' + m.groups.mo"),
            "2024/05"
        );
        assert_eq!(num("var m = /(?<x>a)/.exec('a'); m.index"), 0.0);
        // lookahead
        assert!(b("/a(?=b)/.test('ab')"));
        assert!(!b("/a(?=b)/.test('ac')"));
        assert!(b("/a(?!b)/.test('ac')"));
        assert_eq!(s("'a1b2'.match(/[a-z](?=\\d)/)[0]"), "a");
    }

    #[test]
    fn regex_case_insensitive() {
        assert!(b("/AbC/i.test('xxaBcxx')"));
        assert!(b("/[a-z]+/i.test('HELLO')"));
        assert!(b("new RegExp('x', 'i').test('X')"));
    }

    #[test]
    fn regex_exec_result_and_lastindex() {
        assert!(b("var m = /x/.exec('nope'); m === null"));
        assert_eq!(s("var m = /b/.exec('abc'); m[0]"), "b");
        assert_eq!(num("var m = /b/.exec('abc'); m.length"), 1.0);
        // global exec advances lastIndex
        assert_eq!(
            num("var re = /a/g; re.exec('aa'); re.lastIndex"),
            1.0
        );
        assert_eq!(
            num("var re = /a/g; re.exec('aa'); re.exec('aa'); re.lastIndex"),
            2.0
        );
        // exhausted → null + lastIndex reset to 0
        assert_eq!(
            num("var re = /a/g; re.exec('aa'); re.exec('aa'); re.exec('aa'); re.lastIndex"),
            0.0
        );
        // test() also advances
        assert_eq!(
            num("var re = /b/g; re.test('ab'); re.test('ab'); re.lastIndex"),
            0.0
        );
        // lastIndex honored as start
        assert_eq!(
            num("var re = /a/g; re.lastIndex = 2; var m = re.exec('aaxa'); m.index"),
            3.0
        );
        // non-global ignores lastIndex
        assert_eq!(num("var re = /a/; re.lastIndex = 5; var m = re.exec('a'); m.index"), 0.0);
    }

    #[test]
    fn regex_object_props() {
        assert_eq!(s("/a+/gi.source"), "a+");
        assert_eq!(s("/a+/gi.flags"), "gi");
        assert_eq!(s("/a+/yims.toString()"), "/a+/imsy");
        assert!(b("/x/g.global"));
        assert!(!b("/x/.global"));
        assert!(b("/x/i.ignoreCase"));
        assert!(b("typeof /x/.test === 'function'"));
        assert_eq!(s("var r = new RegExp('\\\\d+', 'g'); r.source"), "\\d+");
        assert_eq!(s("var r = RegExp('a+'); r.test('aaa') ? 'y' : 'n'"), "y");
        // literal evaluates to RegExp instance
        assert!(b("/x/ instanceof RegExp"));
        assert_eq!(s("/x/.toString()"), "/x/");
    }

    #[test]
    fn string_match_regex() {
        // non-global → exec result
        assert_eq!(s("var m = 'a1b2'.match(/\\d/); m[0] + '@' + m.index"), "1@1");
        // global → all match strings
        assert_eq!(s("'a1b2c3'.match(/\\d/g).join(',')"), "1,2,3");
        assert_eq!(num("'hello'.match(/z/g) === null ? 1 : 0"), 1.0);
        assert_eq!(num("'aXbXc'.match(/X/g).length"), 2.0);
        // match with groups+global gives whole matches only
        assert_eq!(s("'a1b2'.match(/(\\w)(\\d)/g).join(',')"), "a1,b2");
        // matchAll → iterator of exec results
        assert_eq!(
            s("var out = []; for (var m of 'a1b2'.matchAll(/(\\w)(\\d)/g)) out.push(m[2]); out.join(',')"),
            "1,2"
        );
        // matchAll requires global
        assert!(eval("'x'.matchAll(/x/)").is_err());
        // string arg is a pattern
        assert_eq!(s("'abc'.match('b')[0]"), "b");
        // Symbol.match plumbing: non-g → exec result, g → all matches
        assert_eq!(s("/\\d+/[Symbol.match]('a12b34')[0]"), "12");
        assert_eq!(s("/\\d+/g[Symbol.match]('a12b34').join('-')"), "12-34");
    }

    #[test]
    fn string_search_regex() {
        assert_eq!(num("'hello'.search(/l+/)"), 2.0);
        assert_eq!(num("'hello'.search(/z/)"), -1.0);
        assert_eq!(num("'aXb'.search(/x/i)"), 1.0);
        assert_eq!(num("'hello'.search('l+')"), 2.0); // string arg as pattern
    }

    #[test]
    fn string_replace_regex() {
        // basic non-global: first only
        assert_eq!(s("'aaa'.replace(/a/, 'b')"), "baa");
        // global: all
        assert_eq!(s("'aaa'.replace(/a/g, 'b')"), "bbb");
        // the blocker pattern
        assert_eq!(s("'k=v:x'.replace(/[=:]/g, '-')"), "k-v-x");
        // capture groups in replacement
        assert_eq!(s("'2024-05'.replace(/(\\d+)-(\\d+)/, '$2/$1')"), "05/2024");
        // $& = whole match, $` = before, $' = after
        assert_eq!(s("'abc'.replace(/b/, '[$&]')"), "a[b]c");
        assert_eq!(s("'abc'.replace(/b/, '<$`>')"), "a<a>c");
        assert_eq!(s("'abc'.replace(/b/, '<$\\'>')"), "a<c>c");
        assert_eq!(s("'abc'.replace(/b/, '$$x')"), "a$xc");
        // function replacer (match, p1..pn, offset, string)
        assert_eq!(
            s("'a=1'.replace(/([a-z])=(\\d)/g, function(m, k, v) { return k + ':' + (v * 2); })"),
            "a:2"
        );
        assert_eq!(
            s("'x1y2'.replace(/\\d/g, function(m, off) { return '(' + m + '@' + off + ')'; })"),
            "x(1@1)y(2@3)"
        );
        // named group $<name>
        assert_eq!(
            s("'a5'.replace(/(?<d>\\d)/, '#$<d>#')"),
            "a#5#"
        );
        // replaceAll with regex needs /g
        assert_eq!(s("'a1b2'.replaceAll(/\\d/g, '_')"), "a_b_");
        assert!(eval("'x'.replaceAll(/x/, 'y')").is_err());
        // replace with no match → same string
        assert_eq!(s("'abc'.replace(/z/, 'x')"), "abc");
        // empty-match replace
        assert_eq!(s("'ab'.replace(/x*/g, '-')"), "-a-b-");
    }

    #[test]
    fn string_split_regex() {
        assert_eq!(s("'a1b2c'.split(/\\d/).join('|')"), "a|b|c");
        assert_eq!(s("'a  b   c'.split(/\\s+/).join('|')"), "a|b|c");
        // captures are spliced in per spec
        assert_eq!(s("'a1b2'.split(/(\\d)/).join('|')"), "a|1|b|2|");
        // limit
        assert_eq!(num("'a1b2c3'.split(/\\d/, 2).length"), 2.0);
        // no match → whole string
        assert_eq!(s("'abc'.split(/z/).join('|')"), "abc");
        // string sep still works
        assert_eq!(num("'a,b,c'.split(',').length"), 3.0);
        assert_eq!(s("'aXbXc'.split('X').join('-')"), "a-b-c");
    }

    #[test]
    fn regex_sticky_and_more() {
        // sticky: anchored at lastIndex
        assert_eq!(num("var re = /b/y; re.lastIndex = 1; var m = re.exec('abc'); m === null ? -1 : m.index"), 1.0);
        assert!(b("var re = /b/y; re.lastIndex = 0; re.exec('abc') === null"));
        // unicode + d flags tolerated
        assert!(b("/a/u.test('a')"));
        assert!(b("/a/d.test('a')"));
        // dot-all + multiline combo
        assert!(b("/a.b/s.test('a\\nb')"));
        // toPrimitive/toString concat
        assert_eq!(s("'' + /x/g"), "/x/g");
    }

    #[test]
    fn regex_replace_fn_args() {
        // fn replacer receives (match, p1..pn, offset, string[, groups])
        assert_eq!(
            s("'ab'.replace(/(a)(b)/, function(m, p1, p2, off, str) { return p1 + off + p2 + str.length; })"),
            "a0b2"
        );
        // named groups → 5th arg is the groups object
        assert_eq!(
            s("'a1'.replace(/(?<d>\\d)/, function(m, g, off, s, gr) { return gr.d; })"),
            "a1"
        );
        // unmatched group → undefined
        assert_eq!(
            s("'ab'.replace(/(x)?b/, function(m, g) { return g === undefined ? 'U' : g; })"),
            "aU"
        );
    }

    #[test]
    fn regex_more_lexer_contexts() {
        // `in`/`instanceof`/`typeof`/`void`/`delete`/`new`/`throw` → regex
        assert_eq!(s("var x = typeof /a/; x"), "object");
        assert_eq!(s("typeof /a/"), "object");
        assert!(b("var x = void /a/; x === undefined"));
        assert!(b("var o = {a:1}; delete /x/.test('x') && delete o.a; !('a' in o)"));
        // after `case`, `do`, `else`
        assert_eq!(num("var r = 0; if (false) {} else /a/.test('a') ? r = 5 : 0; r"), 5.0);
        assert!(b("do { } while (false); /z/.test('z')"));
        // division vs regex after number/paren/bracket
        assert_eq!(num("var a = [10]; a[0] / 2"), 5.0);
        assert_eq!(num("var n = 9; n / 3"), 3.0);
        // regex at start of file
        assert!(b("/fi/.test('first')"));
    }

    #[test]
    fn regex_named_groups_detail() {
        // groups prop is undefined for unnamed regexes
        assert!(b("var m = /a/.exec('a'); m.groups === undefined"));
        // named group missing → undefined value
        assert_eq!(
            s("var m = /(?<a>x)|(?<b>y)/.exec('y'); String(m.groups.a) + String(m.groups.b)"),
            "undefinedy"
        );
        // named backref \\k<name>
        assert!(b("/(?<w>ab)\\k<w>/.test('abab')"));
    }

    #[test]
    fn regex_char_classes_detail() {
        assert!(b("/[\\d]/.test('5')"));
        assert!(!b("/[\\D]/.test('5')"));
        assert!(b("/[a-]/.test('-')"));      // trailing dash literal
        assert!(b("/[-a]/.test('-')"));      // leading dash literal
        assert!(b("/[\\]]/.test(']')"));
        assert!(b("/[a-cx-z]/.test('y')"));
        assert!(!b("/[a-cx-z]/.test('w')"));
        assert!(b("/[\\x41-\\x43]/.test('B')")); // hex range
    }

    #[test]
    fn regex_anchors_and_wordb() {
        assert!(b("/^ab$/.test('ab')"));
        assert!(!b("/^ab$/.test('xab')"));
        assert!(b("/^b/m.test('a\\nb')"));
        assert!(b("/a$/m.test('a\\nb')"));
        assert_eq!(s("'foo bar'.replace(/\\bbar\\b/, 'B')"), "foo B");
        assert_eq!(num("'the cat'.search(/\\bcat\\b/)"), 4.0);
        assert!(b("/\\Bcat/.test('scat')"));
    }

    #[test]
    fn regex_group_capture_detail() {
        // nested groups numbered left-to-right by open paren
        assert_eq!(
            s("var m = /((a)(b))/.exec('ab'); m[1] + ',' + m[2] + ',' + m[3]"),
            "ab,a,b"
        );
        // unmatched group → undefined
        assert!(b("var m = /(x)?b/.exec('b'); m[1] === undefined"));
        // lastIndex advances to match end
        assert_eq!(num("var re = /\\d/g; re.exec('12'); re.lastIndex"), 1.0);
        // sticky exec at exact position
        assert_eq!(num("var re = /\\d/y; re.lastIndex = 2; var m = re.exec('ab3'); m === null ? -9 : m.index"), 2.0);
    }

    #[test]
    fn regex_split_with_captures() {
        assert_eq!(s("'a1b2c'.split(/(\\d)/).join('|')"), "a|1|b|2|c");
        assert_eq!(num("'a1b2'.split(/\\d/).length"), 3.0);
        // multiple capture groups spliced
        assert_eq!(s("'k=1'.split(/(\\w)(=)/).join('|')"), "|k|=|1");
        // trailing separator → trailing empty string
        assert_eq!(s("'aXbX'.split(/X/).join('|')"), "a|b|");
        assert_eq!(s("'XaXb'.split(/X/).join('|')"), "|a|b");
    }

    #[test]
    fn regex_compile_errors() {
        // invalid flag → SyntaxError thrown
        assert!(eval("var r = /x/q;").is_err());
        assert!(eval("new RegExp('a', 'zz')").is_err());
        assert!(eval("new RegExp('(')").is_err());
        // unknown named backref
        assert!(eval("new RegExp('\\\\k<nope>')").is_err());
    }

    #[test]
    fn regex_lookaround() {
        assert!(b("/foo(?=bar)/.test('foobar')"));
        assert!(!b("/foo(?=bar)/.test('foobaz')"));
        assert!(b("/foo(?!bar)/.test('foobaz')"));
        // lookbehind
        assert!(b("/(?<=x)y/.test('xy')"));
        assert!(!b("/(?<=x)y/.test('zy')"));
        assert!(b("/(?<!x)y/.test('zy')"));
        // captures inside lookahead are usable
        assert_eq!(s("'ab'.match(/a(?=(b))/)[1]"), "b");
    }

    #[test]
    fn regex_matchall_iterator() {
        assert_eq!(
            s("var out = ''; for (var m of 'a1b2'.matchAll(/[a-z]\\d/g)) out += m[0]; out"),
            "a1b2"
        );
        // matchAll on regex with named groups
        assert_eq!(
            s("var out = []; for (var m of 'x=1;y=2'.matchAll(/(?<k>\\w)=(?<v>\\d)/g)) out.push(m.groups.k + m.groups.v); out.join(',')"),
            "x1,y2"
        );
    }

    #[test]
    fn regex_misc_semantics() {
        // toString on empty pattern → /(?:)/
        assert_eq!(s("new RegExp().toString()"), "/(?:)/");
        // regex from regex: new RegExp(re, flags)
        assert_eq!(s("var r = new RegExp(/ab/, 'i'); r.flags + r.source"), "iab");
        // lastIndex writable
        assert_eq!(num("var re = /a/g; re.lastIndex = 99; re.exec('a'); re.lastIndex"), 0.0);
        // replace $1 with unmatched group → empty
        assert_eq!(s("'ab'.replace(/(x)?b/, '[$1]')"), "a[]");
        // $n where n > groups → literal
        assert_eq!(s("'ab'.replace(/b/, '$9')"), "a$9");
        // str.match with null result
        assert!(b("'x'.match(/z/) === null"));
        // charCode-driven: \\0 escape
        assert!(b("/\\0x/.test('\\u0000x')"));
    }

    #[test]
    fn regex_catastrophic_backtrack_guard() {
        // classic exponential pattern — must return quickly (step cap), not hang
        let t = std::time::Instant::now();
        let r = eval("var re = /(a+)+$/; re.test('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaX')");
        assert!(r.is_ok());
        assert!(matches!(r.unwrap(), Value::Bool(false)));
        assert!(t.elapsed().as_secs() < 20);
        // another: nested quantifier over long-ish input
        let r = eval("var re = /(.*)*b/; re.test('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa')");
        assert!(r.is_ok());
    }

    #[test]
    fn regex_edge_patterns() {
        // empty pattern → char split (per spec sticky-exec algorithm)
        assert_eq!(num("'ab'.split(/(?:)/).length"), 2.0);
        assert_eq!(s("'ab'.split(/(?:)/).join('|')"), "a|b");
        assert_eq!(s("'ab'.split(/b?/).join('|')"), "a|");
        assert_eq!(s("'axb'.split(/x*/).join('|')"), "a|b");
        assert_eq!(s("'aXbX'.split(/X/).join('|')"), "a|b|");
        // empty class matches nothing
        assert!(!b("/[]/.test('a')"));
        assert!(b("/[^]/.test('a')"));
        // alternation with empty arm
        assert!(b("/a|/.test('zzz')"));
        // regex in template / call args / arrays / objects
        assert!(b("var f = (r) => r.test('a'); f(/a/)"));
        assert!(b("var arr = [/a/, /b/]; arr[0].test('a') && arr[1].test('b')"));
        // regex literal inside class method body
        assert_eq!(
            num("class C { ok() { return /\\d/.test('5') ? 1 : 0; } } new C().ok()"),
            1.0
        );
        // case keyword
        assert_eq!(
            num("var r = 0; switch (true) { case /a/.test('a'): r = 1; break; } r"),
            1.0
        );
    }

    #[test]
    fn regex_from_bundle_patterns() {
        // patterns actually seen in minified bundles
        assert!(b("var j = /\\/+/g; j.test('a//b')"));
        assert_eq!(s("'a=b:c'.replace(/[=:]/g, function(m) { return m === '=' ? '1' : '2'; })"), "a1b2c");
        assert!(b("/^https?:\\/\\//.test('https://x.com')"));
        assert!(b("/[\\w-]+/.test('some-thing_9')"));
        assert_eq!(s("'fooBar'.replace(/([A-Z])/g, '-$1').toLowerCase()"), "foo-bar");
        assert!(b("/function\\s*\\(/.test('function (x)')"));
        assert_eq!(num("'color: rgb(1, 2, 3)'.match(/rgb\\((\\d+), (\\d+), (\\d+)\\)/).length"), 4.0);
        assert_eq!(
            s("'a\\nb\\nc'.split(/\\n/).join(';')"),
            "a;b;c"
        );
        // common idiom: strip trailing slash
        assert_eq!(s("'path/'.replace(/\\/+$/, '')"), "path");
        // camelCase → kebab via function
        assert_eq!(
            s("'backgroundColor'.replace(/[A-Z]/g, function(c) { return '-' + c.toLowerCase(); })"),
            "background-color"
        );
    }

    #[test]
    fn error_cases() {
        assert!(eval("let =").is_err()); // parse error
        assert!(eval("1 +").is_err()); // parse error
        assert!(eval("undeclared_var").is_err()); // ReferenceError
        assert!(eval("null.x").is_err()); // TypeError
        assert!(eval("undefined.f()").is_err());
        assert!(eval("({})()").is_err()); // not a function... actually {} is obj → not callable
        let e = match eval("throw new Error('boom')") {
            Err(e) => e,
            Ok(_) => panic!("expected throw"),
        };
        assert!(e.msg.contains("boom"));
        // stack overflow guarded — run on a big-stack thread so the
        // interpreter's own depth limit kicks in before Rust's stack ends
        let h = std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(|| {
                let mut ctx = Context::new();
                ctx.eval("function f() { return f(); } f()").is_err()
            })
            .unwrap();
        assert!(h.join().unwrap());
    }

    #[test]
    fn asi() {
        assert_eq!(num("let a = 1\nlet b = 2\na + b"), 3.0);
        assert_eq!(num("function f() {\nreturn\n5\n}\nf() ?? 7"), 7.0); // ASI after return
        assert_eq!(num("let x = 5\nx++\nx"), 6.0);
    }

    #[test]
    fn string_iter_and_generators() {
        assert_eq!(num("let n = 0; for (let c of 'abc') n++; n"), 3.0);
        assert_eq!(num("[...'abc'].length"), 3.0);
        assert_eq!(num("let [a, b] = 'xy'; a + b === 'xy' ? 1 : 0"), 1.0);
    }

    #[test]
    fn instanceof_in_delete() {
        assert!(b("'length' in []"));
        assert!(b("0 in [9]"));
        assert!(!b("5 in [9]"));
        assert!(b("'a' in {a: 1}"));
        assert_eq!(num("let a = [1,2,3]; delete a[1]; a[1] === undefined ? 1 : 0"), 1.0);
        assert_eq!(num("let a = [1,2,3]; delete a[1]; a.length"), 3.0);
    }

    #[test]
    fn deep_bundled_patterns() {
        // CommonJS-ish wrapper: (function(module, exports) {...})({exports:{}}, obj.exports)
        let src = r#"
            var bundle = {};
            (function(modules) {
                var cache = {};
                function require(id) {
                    if (cache[id]) return cache[id].exports;
                    var module = { exports: {} };
                    cache[id] = module;
                    modules[id].call(module.exports, module, module.exports, require);
                    return module.exports;
                }
                bundle.require = require;
            })({
                0: function(module, exports, require) {
                    var util = require(1);
                    module.exports = { answer: util.double(21) };
                },
                1: function(module, exports) {
                    exports.double = function(n) { return n * 2; };
                }
            });
            bundle.require(0).answer
        "#;
        assert_eq!(num(src), 42.0);
    }
}
