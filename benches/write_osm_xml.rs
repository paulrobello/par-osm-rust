//! ARC-018: criterion bench for the OSM XML write hot path.
//!
//! Exercises [`write_osm_xml_string`] on a sizable synthetic [`OsmData`] built
//! via the public `OsmData::new` constructor with nodes, ways, and POI nodes.
//! Pure (no network, no `blocking` feature), so it compiles under both
//! `--all-features` and `--no-default-features`.

use std::collections::HashMap;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use par_osm_rust::osm::{
    FeatureSource, OsmData, OsmNode, OsmPoiNode, OsmWay, write_osm_xml_string,
};

/// Build a synthetic `OsmData` with `node_count` nodes, `way_count` ways
/// (each referencing three consecutive nodes), and `poi_count` POI nodes.
/// Constructed entirely via the public `OsmData::new` constructor.
fn build_osm_data(node_count: i64, way_count: i64, poi_count: usize) -> OsmData {
    let nodes: HashMap<i64, OsmNode> = (1..=node_count)
        .map(|i| {
            let frac = i as f64 / (node_count as f64 + 1.0);
            (
                i,
                OsmNode {
                    lat: frac,
                    lon: frac,
                },
            )
        })
        .collect();
    let ways: Vec<OsmWay> = (1..=way_count)
        .map(|w| OsmWay {
            id: w,
            tags: HashMap::from([("highway".to_string(), "residential".to_string())]),
            node_refs: (0..3)
                .map(|n| (w - 1) * 3 + 1 + n)
                .filter(|id| *id <= node_count)
                .collect(),
        })
        .collect();
    let poi_nodes: Vec<OsmPoiNode> = (0..poi_count)
        .map(|i| OsmPoiNode {
            lat: 0.5 + (i as f64) * 1e-6,
            lon: 0.5 + (i as f64) * 1e-6,
            tags: HashMap::from([
                ("amenity".to_string(), "restaurant".to_string()),
                ("name".to_string(), format!("Place {i}")),
            ]),
            source: FeatureSource::Osm,
        })
        .collect();
    OsmData::default()
        .with_nodes(nodes)
        .with_ways(ways)
        .with_bounds(Some((0.0, 0.0, 1.0, 1.0)))
        .with_poi_nodes(poi_nodes)
}

fn bench_write_osm_xml_string(c: &mut Criterion) {
    let mut group = c.benchmark_group("write_osm_xml_string");
    for &node_count in &[1_000i64, 3_000, 10_000] {
        let way_count = node_count / 5;
        let poi_count = (node_count as usize) / 10;
        let data = build_osm_data(node_count, way_count, poi_count);
        group.bench_with_input(BenchmarkId::from_parameter(node_count), &data, |b, data| {
            b.iter(|| {
                let xml = write_osm_xml_string(data);
                assert!(xml.contains("</osm>"));
            });
        });
    }
    group.finish();
}

fn bench_build_osm_data_baseline(c: &mut Criterion) {
    // Baseline: measure OsmData construction alone (the public constructor
    // seeds `ways_by_id` from each `OsmWay::id`), so the write bench can be
    // read as "serialize cost above construction" if needed. Not a perf target.
    let mut group = c.benchmark_group("build_osm_data");
    for &node_count in &[1_000i64, 3_000, 10_000] {
        let way_count = node_count / 5;
        let poi_count = (node_count as usize) / 10;
        group.bench_with_input(
            BenchmarkId::from_parameter(node_count),
            &(node_count, way_count, poi_count),
            |b, &(node_count, way_count, poi_count)| {
                b.iter(|| build_osm_data(node_count, way_count, poi_count));
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_write_osm_xml_string,
    bench_build_osm_data_baseline
);
criterion_main!(benches);
