//! Official [jmespath.test](https://github.com/jmespath/jmespath.test) compliance runner.
//!
//! Fixtures: `tests/fixtures/jmespath/*.json` (vendored from upstream).
//!
//! ## Tiers
//!
//! * **Tier A (must pass 100%)** — core files we claim: `basic`, `current`, `pipe`,
//!   `escape` (quoted identifiers). Any failure fails CI.
//! * **Full suite** — all fixtures; unexpected failures outside the skip list fail CI.
//!   A minimum pass count prevents silent collapse.
//! * **`benchmarks.json`** — excluded from fail (performance shapes; not semantic gates).

use std::fs;
use std::path::{Path, PathBuf};

use jshift::{parse_jmespath_expr, project_jmespath};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct Group {
    given: Value,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    expression: String,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/jmespath")
}

fn load_groups(path: &Path) -> Vec<Group> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn values_equal(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Number(a), Value::Number(b)) => {
            let af = a.as_f64().unwrap_or(f64::NAN);
            let bf = b.as_f64().unwrap_or(f64::NAN);
            af == bf || (af.is_nan() && bf.is_nan())
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| values_equal(x, y))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, v)| b.get(k).is_some_and(|w| values_equal(v, w)))
        }
        _ => actual == expected,
    }
}

/// Known gaps (substring). Shrink over time.
fn is_unsupported(expr: &str) -> bool {
    // Keep this list short and honest — shrink as coverage grows.
    const NEEDLES: &[&str] = &[
        "pad_",
        "from_items",
        "to_items",
        "zip(",
    ];
    NEEDLES.iter().any(|n| expr.contains(n))
}

fn is_tier_a(file: &str) -> bool {
    matches!(
        file,
        "basic.json" | "current.json" | "pipe.json" | "escape.json"
    )
}

fn skip_file(file: &str) -> bool {
    // Performance-oriented cases; not used as semantic gates.
    file == "benchmarks.json"
}

#[derive(Default)]
struct Stats {
    pass: usize,
    error_ok: usize,
    unsupported: usize,
    unexpected: Vec<String>,
    tier_a_unexpected: Vec<String>,
}

fn run_file(path: &Path, stats: &mut Stats) {
    let file = path.file_name().unwrap().to_string_lossy().into_owned();
    if skip_file(&file) {
        return;
    }
    let tier_a = is_tier_a(&file);
    let groups = load_groups(path);
    for (gi, g) in groups.iter().enumerate() {
        let given = serde_json::to_vec(&g.given).expect("serialize given");
        for (ci, case) in g.cases.iter().enumerate() {
            let label = format!("{file}[{gi}/{ci}] {}", case.expression);

            if is_unsupported(&case.expression) {
                stats.unsupported += 1;
                continue;
            }

            if case.error.is_some() {
                let ok_err = match parse_jmespath_expr(&case.expression) {
                    Err(_) => true,
                    Ok(_) => project_jmespath(&given, &case.expression).is_err(),
                };
                if ok_err {
                    stats.error_ok += 1;
                } else {
                    let msg = format!("{label}: expected error, evaluation succeeded");
                    if tier_a {
                        stats.tier_a_unexpected.push(msg.clone());
                    }
                    stats.unexpected.push(msg);
                }
                continue;
            }

            let expected = case.result.clone().unwrap_or(Value::Null);
            match project_jmespath(&given, &case.expression) {
                Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                    Ok(actual) if values_equal(&actual, &expected) => {
                        stats.pass += 1;
                    }
                    Ok(actual) => {
                        let msg = format!(
                            "{label}: result mismatch\n  expected: {expected}\n  actual:   {actual}"
                        );
                        if tier_a {
                            stats.tier_a_unexpected.push(msg.clone());
                        }
                        stats.unexpected.push(msg);
                    }
                    Err(e) => {
                        let msg = format!(
                            "{label}: output not JSON ({e}): {:?}",
                            String::from_utf8_lossy(&bytes)
                        );
                        if tier_a {
                            stats.tier_a_unexpected.push(msg.clone());
                        }
                        stats.unexpected.push(msg);
                    }
                },
                Err(e) => {
                    let msg = format!("{label}: project error: {e}");
                    if tier_a {
                        stats.tier_a_unexpected.push(msg.clone());
                    }
                    stats.unexpected.push(msg);
                }
            }
        }
    }
}

#[test]
fn jmespath_compliance_tier_a_must_pass() {
    let dir = fixtures_dir();
    assert!(dir.is_dir(), "missing {}", dir.display());

    let mut stats = Stats::default();
    for name in ["basic.json", "current.json", "pipe.json", "escape.json"] {
        run_file(&dir.join(name), &mut stats);
    }

    eprintln!(
        "tier A: pass={} error_ok={} unsupported={} unexpected={}",
        stats.pass,
        stats.error_ok,
        stats.unsupported,
        stats.tier_a_unexpected.len()
    );

    if !stats.tier_a_unexpected.is_empty() {
        for line in stats.tier_a_unexpected.iter().take(30) {
            eprintln!("TIER-A FAIL: {line}");
        }
        panic!(
            "tier A compliance failed: {} unexpected",
            stats.tier_a_unexpected.len()
        );
    }
    assert!(stats.pass >= 15, "tier A too few passes: {}", stats.pass);
}

#[test]
fn jmespath_compliance_full_suite_gate() {
    let dir = fixtures_dir();
    let mut stats = Stats::default();

    let mut files: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();

    for path in &files {
        run_file(path, &mut stats);
    }

    eprintln!(
        "full suite: pass={} error_ok={} unsupported={} unexpected={}",
        stats.pass,
        stats.error_ok,
        stats.unsupported,
        stats.unexpected.len()
    );

    // Floors — full official suite (ex-benchmarks) must pass with zero residuals.
    assert!(
        stats.pass >= 740,
        "expected >= 740 value passes, got {}",
        stats.pass
    );
    assert!(
        stats.pass + stats.error_ok >= 880,
        "expected >= 880 pass+error_ok, got {}",
        stats.pass + stats.error_ok
    );
    if !stats.unexpected.is_empty() {
        let show = stats.unexpected.len().min(40);
        for line in stats.unexpected.iter().take(show) {
            eprintln!("FAIL: {line}");
        }
        if stats.unexpected.len() > show {
            eprintln!("... and {} more", stats.unexpected.len() - show);
        }
        panic!(
            "full suite has {} residual mismatches (must be 0)",
            stats.unexpected.len()
        );
    }
}
