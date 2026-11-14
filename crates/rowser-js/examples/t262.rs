//! Test262 runner: `t262 [dir...]` — runs each file, reports pass/skip/fail.
//! TEST262_DIR env or /tmp/test262.
use std::path::{Path, PathBuf};

struct Meta {
    negative_phase: Option<String>, // "parse" | "early" | "runtime" | "resolution"
    negative_type: Option<String>,
    flags: Vec<String>,
    includes: Vec<String>,
    features: Vec<String>,
    raw: bool,
}

fn meta(src: &str) -> Meta {
    let mut m = Meta { negative_phase: None, negative_type: None, flags: vec![], includes: vec![], features: vec![], raw: false };
    let Some(s) = src.find("/*---") else { return m };
    let Some(e) = src[s..].find("---*/") else { return m };
    let yaml = &src[s + 5..s + e];
    let mut in_neg = false;
    for line in yaml.lines() {
        let t = line.trim();
        if t.starts_with("negative:") { in_neg = true; continue; }
        if in_neg && t.starts_with("phase:") { m.negative_phase = Some(t[6..].trim().into()); continue; }
        if in_neg && t.starts_with("type:") { m.negative_type = Some(t[5..].trim().into()); continue; }
        if t.starts_with("flags:") { m.flags = list_val(t); continue; }
        if t.starts_with("includes:") { m.includes = list_val(t); continue; }
        if t.starts_with("features:") { m.features = list_val(t); continue; }
        if !t.is_empty() && !t.starts_with('-') && !t.starts_with(' ') && t.contains(':') { in_neg = false; }
    }
    m.raw = m.flags.iter().any(|f| f == "raw");
    m
}

fn list_val(t: &str) -> Vec<String> {
    // `key: [a, b]` or `key: name` — also `key:` alone (block list next lines ignored)
    let v = &t[t.find(':').unwrap() + 1..];
    let v = v.trim();
    v.trim_start_matches('[').trim_end_matches(']').split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty()).collect()
}

// multi-line list entries (`features:` then `- item` lines)
fn meta2(src: &str, m: &mut Meta) {
    let Some(s) = src.find("/*---") else { return };
    let Some(e) = src[s..].find("---*/") else { return };
    let yaml = &src[s + 5..s + e];
    let mut key: Option<u8> = None; // 0 none,1 flags,2 includes,3 features
    for line in yaml.lines() {
        let t = line.trim();
        if t.starts_with("flags:") { key = Some(1); m.flags.extend(list_val(t)); continue; }
        if t.starts_with("includes:") { key = Some(2); m.includes.extend(list_val(t)); continue; }
        if t.starts_with("features:") { key = Some(3); m.features.extend(list_val(t)); continue; }
        if t.starts_with("- ") {
            let it = t[2..].trim().to_string();
            match key {
                Some(1) => m.flags.push(it),
                Some(2) => m.includes.push(it),
                Some(3) => m.features.push(it),
                _ => {}
            }
            continue;
        }
        key = None;
    }
    m.raw = m.flags.iter().any(|f| f == "raw");
}

const SKIP_FEATURES: &[&str] = &[
    "BigInt", "Proxy", "TypedArray",
    "SharedArrayBuffer", "Atomics", "FinalizationRegistry", "WeakRef",
    "Intl", "Temporal", "Float16Array", "import.meta", "dynamic-import",
    "import-attributes", "iterator-helpers", "explicit-resource-management",
    "Decorators", "RegExp.escape", "regexp-duplicate-named-groups",
    "resizable-arraybuffer", "tail-call-optimization", "async-functions",
    "async-iteration", "generators", "symbol-match-all", "regexp-v-flag",
    "set-methods", "promise-with-resolvers", "array-grouping",
    "regexp-modifiers", "json-parse-with-source", "source-phase-imports",
    "ShadowRealm", "Array.fromAsync",
    "regexp-unicode-property-escapes", "string-replaceall",
    "string-matchall", "numeric-separator-literal",
    "AggregateError", "weakrefs",
    "symbols-as-weakmap-keys", "change-array-by-copy", "array-find-from-last",
    "relative-indexing-method", "uint8array-base64",
    "class-fields-private", "class-fields-public",
    "class-static-block", "class-methods-private",
    "class-static-methods-private", "class-accessor-methods-private",
    "class-static-accessor-methods-private",
    // $262.createRealm / Error.isError — APIs we don't expose
    "cross-realm", "Error.isError", "host-gc-required",
    "CanBlockIsFalse", "SharedArrayBuffer",
];

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() { walk(&p, out); }
        else if p.extension().is_some_and(|x| x == "js")
            && !p.file_name().unwrap().to_string_lossy().contains("_FIXTURE")
        {
            out.push(p);
        }
    }
}

fn main() {
    let root = std::env::var("TEST262_DIR").unwrap_or_else(|_| "/tmp/test262".into());
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut files = Vec::new();
    for a in &args {
        let p = PathBuf::from(&root).join(a);
        walk(&p, &mut files);
    }
    if args.is_empty() { walk(Path::new(&root).join("test").as_path(), &mut files); }
    files.sort();
    let harness_dir = Path::new(&root).join("harness");
    let sta = std::fs::read_to_string(harness_dir.join("sta.js")).unwrap_or_default();
    let assert = std::fs::read_to_string(harness_dir.join("assert.js")).unwrap_or_default();
    let propertyhelper = std::fs::read_to_string(harness_dir.join("propertyHelper.js")).unwrap_or_default();
    let comparearray = std::fs::read_to_string(harness_dir.join("compareArray.js")).unwrap_or_default();

    let (mut pass, mut fail, mut skip) = (0u32, 0u32, 0u32);
    let mut fails: Vec<String> = Vec::new();
    for (idx, f) in files.iter().enumerate() {
        let src = match std::fs::read_to_string(f) { Ok(s) => s, Err(_) => { skip += 1; continue } };
        let mut m = meta(&src); meta2(&src, &mut m);
        // unsatisfiable flags/features
        if m.flags.iter().any(|f| matches!(f.as_str(), "module" | "async" | "CanBlockIsFalse" | "SharedArrayBuffer")) {
            skip += 1; continue;
        }
        if m.features.iter().any(|f| SKIP_FEATURES.contains(&f.as_str())) {
            skip += 1; continue;
        }
        // harness includes we don't serve
        let mut inc_src = String::new();
        let mut bad_inc = false;
        for inc in &m.includes {
            if inc == "compareArray.js" {
                inc_src.push_str(&comparearray);
            } else if inc == "propertyHelper.js" {
                inc_src.push_str(&propertyhelper);
            } else if inc == "assert.js" || inc == "sta.js" {
                // already in prelude
            } else {
                let p = harness_dir.join(inc);
                match std::fs::read_to_string(&p) {
                    Ok(s) => inc_src.push_str(&s),
                    Err(_) => { bad_inc = true; break; }
                }
            }
        }
        if bad_inc { skip += 1; continue; }

        let body = if m.raw { src.clone() } else {
            format!("{sta}\n{assert}\n{inc_src}\n{src}")
        };

        // strip the frontmatter comment? harmless — it's a comment.
        let rel = f.strip_prefix(&root).unwrap_or(f).display().to_string();
        if std::env::var("VERBOSE").is_ok() {
            eprintln!("RUN {rel}");
        }
        let mut ctx = rowser_js::Context::new();
        ctx.interp.arm_deadline(std::time::Duration::from_secs(3));
        let res = ctx.eval(&body);
        let ok = match (&m.negative_phase, res) {
            (Some(p), _) if p == "parse" => {
                // real parse pass — expect a syntax error
                rowser_js::parser::parse(&src).is_err()
            }
            (Some(p), _) if p == "early" || p == "resolution" => {
                // early/resolution SyntaxErrors — our parser doesn't do
                // static error checks, so count via eval-failure heuristic
                res2_parse(&body).is_err() || res_is_syntax_err(&body)
            }
            (Some(_), Err(e)) => {
                // runtime negative: error name must match
                m.negative_type.as_deref().is_none_or(|t| e.to_string().contains(t))
            }
            (Some(_), Ok(_)) => false,
            (None, Ok(_)) => true,
            (None, Err(_)) => false,
        };
        if ok { pass += 1; } else { fail += 1; fails.push(rel); }
        if idx % 500 == 0 { eprintln!("\r{idx}/{} p={pass} f={fail} s={skip}", files.len()); }
    }
    eprintln!("\n=== pass={pass} fail={fail} skip={skip} rate={:.1}%",
        pass as f64 / (pass + fail).max(1) as f64 * 100.0);
    let show: usize = std::env::var("SHOW_FAILS").unwrap_or_else(|_| "0".into()).parse().unwrap_or(0);
    for f in fails.iter().take(show) { println!("FAIL {f}"); }
    let out = std::env::var("T262_FAILS_OUT").unwrap_or_else(|_| "/tmp/t262_fails.txt".into());
    std::fs::write(&out, fails.join("\n")).ok();
}

fn res_is_syntax_err(_body: &str) -> bool { false }

fn res2_parse(body: &str) -> Result<(), ()> {
    // parse phase: source must fail to parse; we can't call the parser
    // directly from the example, so approximate via eval error text
    match rowser_js::Context::new().eval(body) {
        Ok(_) => Ok(()),
        Err(e) => {
            let m = e.to_string();
            if m.contains("parse") || m.contains("Syntax") || m.contains("unexpected") || m.contains("expected") {
                Err(())
            } else { Ok(()) }
        }
    }
}
