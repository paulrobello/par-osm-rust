//! ARC-018: criterion benches for the OSM XML parse hot path.
//!
//! Exercises both [`parse_osm_xml_str`] (in-memory string) and
//! [`parse_osm_xml_file`] (streaming from disk via `BufReader`) on a sizable
//! synthetic OSM XML fixture built inline. Both paths are pure (no network,
//! no `blocking` feature), so the bench compiles under both
//! `--all-features` and `--no-default-features`.

use std::io::Write;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use par_osm_rust::osm::parse_osm_xml_file;
use par_osm_rust::osm::parse_osm_xml_str;
use tempfile::NamedTempFile;

/// Build a synthetic OSM XML document with `node_count` nodes and `way_count`
/// ways. Each way references three consecutive nodes and carries a single
/// `highway=residential` tag, so the fixture exercises the parser's node,
/// `<nd ref>`, and `<tag>` paths at scale.
fn synthetic_osm_xml(node_count: usize, way_count: usize) -> String {
    let mut xml = String::with_capacity(node_count * 48 + way_count * 80);
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
        xml.push_str("    <tag k=\"highway\" v=\"residential\"/>\n");
        xml.push_str("  </way>\n");
    }
    xml.push_str("</osm>\n");
    xml
}

/// Write `contents` to a `.osm` tempfile and return the handle (kept alive so
/// the file exists for the duration of the bench).
fn write_tempfile(contents: &str) -> NamedTempFile {
    let mut tmp = tempfile::Builder::new()
        .suffix(".osm")
        .tempfile()
        .expect("tempfile creation");
    tmp.write_all(contents.as_bytes()).expect("tempfile write");
    tmp
}

fn bench_parse_osm_xml_str(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_osm_xml_str");
    for &size in &[1_000usize, 3_000, 10_000] {
        let xml = synthetic_osm_xml(size, size / 5);
        group.bench_with_input(BenchmarkId::from_parameter(size), &xml, |b, xml| {
            b.iter(|| {
                let data = parse_osm_xml_str(xml).expect("parse_osm_xml_str");
                assert_eq!(data.iter_ways().count(), size / 5);
            });
        });
    }
    group.finish();
}

fn bench_parse_osm_xml_file(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_osm_xml_file");
    for &size in &[1_000usize, 3_000, 10_000] {
        let xml = synthetic_osm_xml(size, size / 5);
        let tmp = write_tempfile(&xml);
        let path = tmp.path();
        group.bench_with_input(BenchmarkId::from_parameter(size), path, |b, path| {
            b.iter(|| {
                let data = parse_osm_xml_file(path).expect("parse_osm_xml_file");
                assert_eq!(data.iter_ways().count(), size / 5);
            });
        });
    }
    group.finish();
}

fn bench_build_fixture_baseline(c: &mut Criterion) {
    // Baseline: measure fixture construction alone, so the parse benches can
    // be read as "parse cost above construction" if needed. Not a perf target.
    let mut group = c.benchmark_group("synthetic_osm_xml_build");
    for &size in &[1_000usize, 3_000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| synthetic_osm_xml(size, size / 5));
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_parse_osm_xml_str,
    bench_parse_osm_xml_file,
    bench_build_fixture_baseline,
);
criterion_main!(benches);
