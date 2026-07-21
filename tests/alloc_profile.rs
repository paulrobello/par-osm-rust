//! ENH-008 measurement gate: a counting global allocator that attributes
//! parse-time allocations, so the tag-key interning win can be quantified
//! **before** any interning code is written (plan: `docs/fable/ENH-008-...`).
//!
//! This is its own integration-test binary so the `#[global_allocator]` below
//! is isolated from the unit-test and criterion binaries — it never perturbs
//! `make test` or the benches. The single test is `#[ignore]`'d for the same
//! reason: it does not run in a normal `cargo test`, only on explicit
//! invocation, which is how the ENH-008 baseline and post-interning deltas are
//! captured.
//!
//! Run:
//! ```text
//! cargo test --test alloc_profile -- --ignored --nocapture
//! ```
//!
//! Gate (per the plan): tag-key allocations must be **≥10%** of total parse
//! allocations. If the measured share is below 10%, ENH-008 is rejected — the
//! assert at the bottom enforces that, and the printed table records the
//! numbers to paste into the PR / `ENHANCEMENTS.md`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use par_osm_rust::osm::parse_osm_xml_str;

/// Counting wrapper over the system allocator. Tracks *allocation operations*
/// (fresh `alloc` + growth `realloc`) and requested bytes — not deallocations,
/// which are not what interning reduces. Relaxed ordering is correct: this is
/// single-threaded measurement (the test runs alone), and we only need the
/// final tally, not cross-thread ordering.
struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static REALLOCS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    // Override realloc so a growing buffer counts as one growth op (and charges
    // its new size) rather than falling through to the default alloc+dealloc
    // pair, which would double-count Vec/HashMap growth.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        REALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn reset_counters() {
    ALLOCS.store(0, Ordering::Relaxed);
    ALLOC_BYTES.store(0, Ordering::Relaxed);
    REALLOCS.store(0, Ordering::Relaxed);
}

fn read_counters() -> (u64, u64, u64) {
    (
        ALLOCS.load(Ordering::Relaxed),
        ALLOC_BYTES.load(Ordering::Relaxed),
        REALLOCS.load(Ordering::Relaxed),
    )
}

/// Build a representative OSM XML document: `node_count` untagged nodes plus
/// `way_count` ways, each referencing three nodes and carrying `tags_per_way`
/// tags drawn from a fixed hot-vocabulary key set with one unique-valued `name`.
///
/// This mirrors real extract shape far better than the single-tag
/// `synthetic_osm_xml` bench fixture: real OSM ways carry 5–15 tags whose keys
/// come from a tiny hot vocabulary (`highway`, `name`, `surface`, …) repeated
/// across millions of ways — exactly the reuse pattern interning targets.
fn representative_osm_xml(node_count: usize, way_count: usize, tags_per_way: usize) -> String {
    // The first `tags_per_way` keys from the plan's COMMON_TAG_KEYS list.
    const HOT_KEYS: &[&str] = &[
        "highway", "name", "surface", "oneway", "lit", "lanes", "maxspeed", "sidewalk", "bridge",
        "tunnel", "ref", "layer",
    ];
    let keys = &HOT_KEYS[..tags_per_way.min(HOT_KEYS.len())];
    let mut xml = String::with_capacity(node_count * 48 + way_count * (80 + keys.len() * 40));
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osm version=\"0.6\">\n");
    xml.push_str("  <bounds minlat=\"0.0\" minlon=\"0.0\" maxlat=\"1.0\" maxlon=\"1.0\"/>\n");
    for i in 1..=node_count {
        let lat = i as f64 / (node_count as f64 + 1.0);
        let lon = i as f64 / (node_count as f64 + 1.0);
        xml.push_str(&format!(
            "  <node id=\"{i}\" lat=\"{lat:.6}\" lon=\"{lon:.6}\"/>\n"
        ));
    }
    for w in 1..=way_count {
        xml.push_str(&format!("  <way id=\"{w}\">\n"));
        let base = ((w - 1) * 3 + 1).min(node_count.saturating_sub(2).max(1));
        for n in base..=(base + 2).min(node_count) {
            xml.push_str(&format!("    <nd ref=\"{n}\"/>\n"));
        }
        // Hot-vocab keys with repeated values, plus a unique `name` per way
        // (the one value that is genuinely distinct in real data).
        for (idx, k) in keys.iter().enumerate() {
            let v = if *k == "name" {
                format!("Way {w}")
            } else {
                ["residential", "asphalt", "yes", "2", "50", "both"][idx % 6].to_string()
            };
            xml.push_str(&format!("    <tag k=\"{k}\" v=\"{v}\"/>\n"));
        }
        xml.push_str("  </way>\n");
    }
    xml.push_str("</osm>\n");
    xml
}

#[test]
#[ignore]
fn measure_key_allocation_share() {
    let (node_count, way_count, tags_per_way) = (60_000, 20_000, 8);
    let xml = representative_osm_xml(node_count, way_count, tags_per_way);

    // Allocation count is deterministic, so a single measured parse suffices.
    // The fixture String is kept alive across the parse so its own allocations
    // are excluded by the reset bracket below (only the parse is charged).
    reset_counters();
    let start = Instant::now();
    let data = parse_osm_xml_str(&xml).expect("parse_osm_xml_str");
    let elapsed = start.elapsed();
    let (allocs, bytes, reallocs) = read_counters();

    // Walk the parsed result to count tags and distinct keys. At the pre-interning
    // baseline each key was a fresh `String` allocation, so `n_tags` equaled the
    // key-allocation count; after interning, only the ~8 distinct keys allocate.
    // The apples-to-apples before/after signal is `total_ops` / `total_bytes`
    // above, not the key-share line below (which is the gate metric, not a delta).
    let mut n_tags: u64 = 0;
    let mut key_bytes: u64 = 0;
    let mut distinct_keys: HashSet<&str> = HashSet::new();
    let n_ways = data.iter_ways().count();
    for w in data.iter_ways() {
        for k in w.tags.keys() {
            n_tags += 1;
            key_bytes += k.len() as u64;
            distinct_keys.insert(&**k);
        }
    }

    let total_ops = allocs + reallocs;
    let key_share_count = n_tags as f64 / total_ops as f64;
    let key_share_bytes = key_bytes as f64 / bytes as f64;
    let bytes_mib = bytes as f64 / (1024.0 * 1024.0);

    println!();
    println!("================ ENH-008 ALLOCATION GATE ================");
    println!("fixture            : {node_count} nodes, {way_count} ways × {tags_per_way} tags/way");
    println!("ways parsed        : {n_ways}");
    println!("tags parsed        : {n_tags}");
    println!(
        "distinct tag keys  : {} (reuse ratio {:.1}×)",
        distinct_keys.len(),
        n_tags as f64 / distinct_keys.len() as f64
    );
    println!("--------------------------------------------------------");
    println!("total alloc ops    : {total_ops:>14} (allocs {allocs} + reallocs {reallocs})");
    println!("total bytes        : {bytes:>14} ({bytes_mib:.1} MiB requested)");
    println!("tag key allocs     : {n_tags:>14}  (= #tags; each key is one String)");
    println!("tag key bytes      : {key_bytes:>14}");
    println!("--------------------------------------------------------");
    println!(
        "KEY SHARE by count : {:>6.2}%  ({} / {})",
        key_share_count * 100.0,
        n_tags,
        total_ops
    );
    println!("KEY SHARE by bytes : {:>6.2}%", key_share_bytes * 100.0);
    println!("parse wall time    : {:.3} s", elapsed.as_secs_f64());
    println!("========================================================");

    // ENH-008 gate: keys must account for ≥10% of parse allocations for
    // interning to be worth the API churn. Asserted so a CI run surfaces a
    // regression, but the table above is the artifact of record.
    const GATE: f64 = 0.10;
    assert!(
        key_share_count >= GATE,
        "ENH-008 gate FAILED: key allocations are {:.2}% of parse ops, below the {:.0}% threshold — reject interning",
        key_share_count * 100.0,
        GATE * 100.0
    );
    println!(
        "GATE: PASS (key share {:.2}% ≥ {:.0}%) — proceed with interning",
        key_share_count * 100.0,
        GATE * 100.0
    );
}
