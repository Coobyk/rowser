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
