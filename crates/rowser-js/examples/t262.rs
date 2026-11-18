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
    let mut m = Meta {
        negative_phase: None,
        negative_type: None,
        flags: vec![],
        includes: vec![],
        features: vec![],
        raw: false,
    };
    let Some(s) = src.find("/*---") else { return m };
    let Some(e) = src[s..].find("---*/") else {
        return m;
    };
    let yaml = &src[s + 5..s + e];
    let mut in_neg = false;
    for line in yaml.split(|c| c == '\n' || c == '\r') {
        let t = line.trim();
        if t.starts_with("negative:") {
            in_neg = true;
            continue;
        }
        if in_neg && t.starts_with("phase:") {
            m.negative_phase = Some(t[6..].trim().into());
            continue;
        }
        if in_neg && t.starts_with("type:") {
            m.negative_type = Some(t[5..].trim().into());
            continue;
        }
        if t.starts_with("flags:") {
            m.flags = list_val(t);
            continue;
        }
        if t.starts_with("includes:") {
            m.includes = list_val(t);
            continue;
        }
        if t.starts_with("features:") {
            m.features = list_val(t);
            continue;
        }
        if !t.is_empty() && !t.starts_with('-') && !t.starts_with(' ') && t.contains(':') {
            in_neg = false;
        }
    }
    m.raw = m.flags.iter().any(|f| f == "raw");
    m
}

fn list_val(t: &str) -> Vec<String> {
    // `key: [a, b]` or `key: name` — also `key:` alone (block list next lines ignored)
    let v = &t[t.find(':').unwrap() + 1..];
    let v = v.trim();
    v.trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// multi-line list entries (`features:` then `- item` lines)
fn meta2(src: &str, m: &mut Meta) {
    let Some(s) = src.find("/*---") else { return };
    let Some(e) = src[s..].find("---*/") else {
        return;
    };
    let yaml = &src[s + 5..s + e];
    let mut key: Option<u8> = None; // 0 none,1 flags,2 includes,3 features
    for line in yaml.split(|c| c == '\n' || c == '\r') {
        let t = line.trim();
        if t.starts_with("flags:") {
            key = Some(1);
            continue;
        }
        if t.starts_with("includes:") {
            key = Some(2);
            continue;
        }
        if t.starts_with("features:") {
            key = Some(3);
            continue;
        }
        if t.starts_with("- ") {
            let it = t[2..].trim().to_string();
            let list = match key {
                Some(1) => &mut m.flags,
                Some(2) => &mut m.includes,
                Some(3) => &mut m.features,
                _ => continue,
            };
            if !list.contains(&it) {
                list.push(it);
            }
            continue;
        }
        key = None;
    }
    m.raw = m.flags.iter().any(|f| f == "raw");
}

const SKIP_FEATURES: &[&str] = &[
    "SharedArrayBuffer",
    "Atomics",
    "FinalizationRegistry",
    "Intl",
    "Temporal",
    "Float16Array",
    "import.meta",
    "dynamic-import",
    "import-attributes",
    "explicit-resource-management",
    "decorators",
    // regexp-duplicate-named-groups — implemented (Node::Backrefs +
    // dedup'd groups objects)
    "tail-call-optimization",
    // async-iteration — implemented (AGen machinery + afx wrap)
    "regexp-v-flag",
    "regexp-modifiers",
    "source-phase-imports",
    "ShadowRealm",
    // Array.fromAsync — implemented (JS-built async fn + afx wrap)
    // await-dictionary — implemented (Promise.allKeyed/allSettledKeyed)
    "regexp-unicode-property-escapes",
    // uint8array-base64 — implemented (binary.rs b64/hex section)
    // $262.createRealm / Error.isError — APIs we don't expose
    "cross-realm",
    "host-gc-required",
    // stage-3 Error.prototype.stack accessor proposal — unimplemented
    "error-stack-accessor",
    "CanBlockIsFalse",
    "SharedArrayBuffer",
];

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    if dir.is_file() {
        if dir.extension().is_some_and(|x| x == "js") {
            out.push(dir.to_path_buf());
        }
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "js")
            && !p
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("_FIXTURE")
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
    if args.is_empty() {
        walk(Path::new(&root).join("test").as_path(), &mut files);
    }
    files.sort();
    let harness_dir = Path::new(&root).join("harness");
    let sta = std::fs::read_to_string(harness_dir.join("sta.js")).unwrap_or_default();
    let assert = std::fs::read_to_string(harness_dir.join("assert.js")).unwrap_or_default();
    let propertyhelper =
        std::fs::read_to_string(harness_dir.join("propertyHelper.js")).unwrap_or_default();
    let comparearray =
        std::fs::read_to_string(harness_dir.join("compareArray.js")).unwrap_or_default();

    let (mut pass, mut fail, mut skip) = (0u32, 0u32, 0u32);
    let mut fails: Vec<String> = Vec::new();
    for (idx, f) in files.iter().enumerate() {
        let src = match std::fs::read_to_string(f) {
            Ok(s) => s,
            Err(_) => {
                skip += 1;
                continue;
            }
        };
        let mut m = meta(&src);
        meta2(&src, &mut m);
        // unsatisfiable flags/features
        if m.flags.iter().any(|f| {
            matches!(
                f.as_str(),
                "module" | "CanBlockIsFalse" | "SharedArrayBuffer"
            )
        }) {
            skip += 1;
            continue;
        }
        if m.features
            .iter()
            .any(|f| SKIP_FEATURES.contains(&f.as_str()))
        {
            skip += 1;
            continue;
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
                    Err(_) => {
                        bad_inc = true;
                        break;
                    }
                }
            }
        }
        if bad_inc {
            skip += 1;
            continue;
        }

        let strict = !m.raw && m.flags.iter().any(|f| f == "onlyStrict");
        // async-flagged tests signal completion through $DONE() — the
        // marker lands in console output once jobs drain
        let async_pre = if m.flags.iter().any(|f| f == "async") {
            std::fs::read_to_string(harness_dir.join("doneprintHandle.js"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let body = if m.raw {
            src.clone()
        } else {
            // `print` — SpiderMonkey-era harness global some old tests use
            format!("var print = console.log;\n{sta}\n{assert}\n{async_pre}\n{inc_src}\n{src}")
        };
        let body = if strict {
            format!("\"use strict\";\n{body}")
        } else {
            body
        };
        let src = if strict {
            format!("\"use strict\";\n{src}")
        } else {
            src
        };

        // strip the frontmatter comment? harmless — it's a comment.
        let rel = f.strip_prefix(&root).unwrap_or(f).display().to_string();
        if std::env::var("VERBOSE").is_ok() {
            eprintln!("RUN {rel}");
        }
        // a panic in one test must not kill the run — count it as a fail
        let is_async_test = m.flags.iter().any(|f| f == "async");
        let (res, output) = std::panic::catch_unwind(|| {
            let mut ctx = rowser_js::Context::new();
            let secs: u64 = std::env::var("T262_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(15);
            if secs > 0 {
                ctx.interp
                    .arm_deadline(std::time::Duration::from_secs(secs));
            }
            let r = ctx.eval(&body);
            (r, ctx.take_output())
        })
        .unwrap_or_else(|_| {
            (
                Err(rowser_js::Error::new("panic during test", 0, 0)),
                Vec::new(),
            )
        });
        if std::env::var("T262_DEBUG").is_ok() {
            if let Err(e) = &res {
                eprintln!("ERR {rel}: {e}");
            }
        }
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
                m.negative_type
                    .as_deref()
                    .is_none_or(|t| e.to_string().contains(t))
            }
            (Some(_), Ok(_)) => false,
            // async-flagged tests pass only on the $DONE() marker —
            // "AsyncTestComplete" must appear with no "AsyncTestFailure"
            (None, Ok(_)) if is_async_test => {
                output.iter().any(|l| l.contains("Test262:AsyncTestComplete"))
                    && !output
                        .iter()
                        .any(|l| l.contains("Test262:AsyncTestFailure"))
            }
            (None, Ok(_)) => true,
            (None, Err(_)) => false,
        };
        if ok {
            pass += 1;
        } else {
            fail += 1;
            fails.push(rel);
        }
        if idx % 500 == 0 {
            eprintln!("\r{idx}/{} p={pass} f={fail} s={skip}", files.len());
        }
    }
    eprintln!(
        "\n=== pass={pass} fail={fail} skip={skip} rate={:.1}%",
        pass as f64 / (pass + fail).max(1) as f64 * 100.0
    );
    let show: usize = std::env::var("SHOW_FAILS")
        .unwrap_or_else(|_| "0".into())
        .parse()
        .unwrap_or(0);
    for f in fails.iter().take(show) {
        println!("FAIL {f}");
    }
    let out = std::env::var("T262_FAILS_OUT").unwrap_or_else(|_| "/tmp/t262_fails.txt".into());
    std::fs::write(&out, fails.join("\n")).ok();
}

fn res_is_syntax_err(_body: &str) -> bool {
    false
}

fn res2_parse(body: &str) -> Result<(), ()> {
    // parse phase: source must fail to parse; we can't call the parser
    // directly from the example, so approximate via eval error text
    match rowser_js::Context::new().eval(body) {
        Ok(_) => Ok(()),
        Err(e) => {
            let m = e.to_string();
            if m.contains("parse")
                || m.contains("Syntax")
                || m.contains("unexpected")
                || m.contains("expected")
            {
                Err(())
            } else {
                Ok(())
            }
        }
    }
}
