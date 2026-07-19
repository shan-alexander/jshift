//! Real-catalog projection against TeeFury `products.json` fixtures.
//!
//! Fixtures are **gitignored** (see `benches/data/`). Fetch locally:
//!
//! ```bash
//! ./scripts/fetch_teefury.sh 4
//! cargo test --test teefury_project -- --nocapture
//! ```
//!
//! When fixtures are missing, tests **skip** (exit 0) so CI without network stays green.

use std::fs;
use std::path::PathBuf;

use jshift::{
    project, project_jmespath, project_paths, projected_len, ProjectPlan, ProjectStyle,
};

fn fixture(page: u32) -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("benches/data")
        .join(format!("teefury_products_p{page}.json"));
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

fn load_page(page: u32) -> Option<Vec<u8>> {
    let p = fixture(page)?;
    Some(fs::read(p).expect("read fixture"))
}

#[test]
fn teefury_page1_path_project_shrinks_and_parses() {
    let Some(json) = load_page(1) else {
        eprintln!("skip teefury_page1_path_project: no fixture (run ./scripts/fetch_teefury.sh)");
        return;
    };
    let in_len = json.len();
    assert!(in_len > 50_000, "expected a real catalog page, got {in_len} bytes");

    let paths = &[
        "products[].id",
        "products[].title",
        "products[].handle",
        "products[].vendor",
        "products[].product_type",
        "products[].variants[].id",
        "products[].variants[].price",
        "products[].variants[].sku",
        "products[].variants[].available",
        "products[].images[].src",
        "products[].images[].width",
        "products[].images[].height",
    ];
    let out = project_paths(&json, paths).expect("project_paths");
    let out_len = out.len();
    let ratio = out_len as f64 / in_len as f64;

    // Must be valid JSON and much smaller (drop body_html / fat blobs).
    let v: serde_json::Value = serde_json::from_slice(&out).expect("projected JSON parses");
    let n = v["products"].as_array().expect("products array").len();
    assert!(n >= 1, "expected products");
    assert!(
        ratio < 0.55,
        "expected substantial shrink, ratio={ratio:.3} in={in_len} out={out_len}"
    );

    // First product card shape
    let p0 = &v["products"][0];
    assert!(p0.get("id").is_some());
    assert!(p0.get("title").is_some());
    assert!(p0.get("body_html").is_none());
    assert!(p0.get("variants").is_some());

    eprintln!(
        "teefury p1 path-project: {in_len} -> {out_len} bytes ({:.1}% of input), {n} products",
        ratio * 100.0
    );
}

#[test]
fn teefury_page1_jmespath_listing_cards() {
    let Some(json) = load_page(1) else {
        eprintln!("skip teefury_page1_jmespath: no fixture");
        return;
    };

    // Catalog → listing cards (JMESPath result is a bare array).
    let expr = "products[*].{id: id, title: title, handle: handle, price: variants[0].price, image: images[0].src}";
    let out = project_jmespath(&json, expr).expect("jmespath project");
    let v: serde_json::Value = serde_json::from_slice(&out).expect("parse cards");
    let cards = v.as_array().expect("array of cards");
    assert!(!cards.is_empty());
    let c0 = &cards[0];
    assert!(c0.get("id").is_some());
    assert!(c0.get("title").is_some());
    // renamed / nested extract
    assert!(c0.get("price").is_some() || c0.get("price") == Some(&serde_json::Value::Null));

    let in_len = json.len();
    let out_len = out.len();
    eprintln!(
        "teefury p1 jmespath cards: {in_len} -> {out_len} bytes, {} cards",
        cards.len()
    );
    assert!(out_len < in_len / 3, "cards should be a small fraction of the page");
}

#[test]
fn teefury_multipage_project_totals() {
    let mut pages = Vec::new();
    for p in 1..=4u32 {
        if let Some(b) = load_page(p) {
            pages.push((p, b));
        }
    }
    if pages.is_empty() {
        eprintln!("skip teefury_multipage: no fixtures");
        return;
    }

    let plan = ProjectPlan::from_jmespath(
        "products[*].{id: id, title: title, handle: handle, vendor: vendor}",
    )
    .unwrap()
    .style(ProjectStyle::Compact);

    let mut in_total = 0usize;
    let mut out_total = 0usize;
    let mut products = 0usize;
    for (page, json) in &pages {
        in_total += json.len();
        let out = project(json, &plan).unwrap();
        out_total += out.len();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        products += v.as_array().map(|a| a.len()).unwrap_or(0);
        let exact = projected_len(json, &plan).unwrap();
        assert_eq!(exact, out.len());
        eprintln!(
            "  page {page}: {} -> {} bytes",
            json.len(),
            out.len()
        );
    }

    eprintln!(
        "teefury multipage ({} pages): {in_total} -> {out_total} bytes, {products} product cards",
        pages.len()
    );
    assert!(out_total < in_total / 2);
    assert!(products >= pages.len()); // at least one product per page present
}

#[test]
fn teefury_pretty_style_still_parses() {
    let Some(json) = load_page(1) else {
        eprintln!("skip teefury_pretty: no fixture");
        return;
    };
    let plan = ProjectPlan::from_jmespath("products[0].{id: id, title: title}")
        .unwrap()
        .style(ProjectStyle::Pretty { indent: 2 });
    let out = project(&json, &plan).unwrap();
    assert!(out.contains(&b'\n'));
    serde_json::from_slice::<serde_json::Value>(&out).unwrap();
}
