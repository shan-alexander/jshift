//! Fuzz keep-list projection and path finds together.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let data = if data.len() > 12_288 {
        &data[..12_288]
    } else {
        data
    };
    if data.len() < 3 {
        let _ = jshift::project_paths(data, &[]);
        return;
    }

    // Split into path blob + json.
    let split = (data[0] as usize % 32) + 1;
    let split = split.min(data.len());
    let path_blob = &data[..split];
    let json = &data[split..];

    let path_str = String::from_utf8_lossy(path_blob);
    let paths: Vec<&str> = path_str
        .split(|c: char| c == '\0' || c == '\n' || c == '|')
        .filter(|s| !s.is_empty())
        .take(8)
        .collect();

    let path_refs: Vec<&str> = if paths.is_empty() {
        vec!["a"]
    } else {
        paths
    };

    let _ = jshift::project_paths(json, &path_refs);
    for p in &path_refs {
        let segs = jshift::parse_path(p);
        let _ = jshift::find_value(json, &segs);
    }
    if let Ok(plan) = jshift::ProjectPlan::from_jmespath("length(@)") {
        let _ = jshift::project(json, &plan);
    }
});
