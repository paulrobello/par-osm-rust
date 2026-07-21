//! ARC-018: criterion bench for the POI-dedupe hot path.
//!
//! [`par_osm_rust::sources::merge_source_data`] internally calls the (private)
//! `dedupe_pois_with_overture_preference` spatial-grid dedupe. Benching it via
//! the public merge entry point exercises the dedupe with two realistic
//! `OsmData` inputs: an OSM source and an Overture source with overlapping
//! POIs. Pure (no network, no `blocking` feature), so it compiles under both
//! `--all-features` and `--no-default-features`.

use std::collections::HashMap;

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use par_osm_rust::osm::{FeatureSource, OsmData, OsmPoiNode};
use par_osm_rust::sources::{PoiSourceMode, merge_source_data};

/// One degree of latitude ≈ this many metres (matches `metres_between` in the
/// crate). Used to space POIs so duplicates fall within the 25 m threshold.
const METRES_PER_DEGREE_LAT: f64 = 111_320.0;

/// Build an `OsmData` carrying `count` POIs of the given source, each at a
/// distinct ~50 m grid position. Constructed via the public `OsmData::new`
/// constructor.
fn data_with_pois(count: usize, source: FeatureSource) -> OsmData {
    let poi_nodes: Vec<OsmPoiNode> = (0..count)
        .map(|i| {
            let lat = (i as f64) * 50.0 / METRES_PER_DEGREE_LAT;
            let lon = (i as f64) * 50.0 / METRES_PER_DEGREE_LAT;
            OsmPoiNode {
                lat,
                lon,
                tags: HashMap::from([
                    ("amenity".into(), "restaurant".to_string()),
                    ("name".into(), format!("Place {i}")),
                ]),
                source,
            }
        })
        .collect();
    OsmData::default()
        .with_bounds(Some((0.0, 0.0, 1.0, 1.0)))
        .with_poi_nodes(poi_nodes)
}

/// Build a matching Overture source whose POIs sit ~5 m north-east of each
/// OSM POI — inside the 25 m duplicate threshold, so the spatial-grid dedupe
/// must collapse each Overture/OSM pair and retain the Overture representative.
fn data_with_overture_duplicates(count: usize) -> OsmData {
    let offset = 5.0 / METRES_PER_DEGREE_LAT;
    let poi_nodes: Vec<OsmPoiNode> = (0..count)
        .map(|i| {
            let lat = (i as f64) * 50.0 / METRES_PER_DEGREE_LAT + offset;
            let lon = (i as f64) * 50.0 / METRES_PER_DEGREE_LAT + offset;
            OsmPoiNode {
                lat,
                lon,
                tags: HashMap::from([
                    ("amenity".into(), "restaurant".to_string()),
                    ("name".into(), format!("Place {i}")),
                ]),
                source: FeatureSource::Overture,
            }
        })
        .collect();
    OsmData::default()
        .with_bounds(Some((0.0, 0.0, 1.0, 1.0)))
        .with_poi_nodes(poi_nodes)
}

fn bench_merge_source_data_dedupe(c: &mut Criterion) {
    let mut group = c.benchmark_group("merge_source_data_dedupe");
    for &count in &[500usize, 2_000, 8_000] {
        // `OsmData` is not `Clone`, and `merge_source_data` consumes both
        // inputs by value, so rebuild fresh inputs per iteration via
        // `iter_batched` (the setup closure runs outside the timed window).
        // Each Overture POI duplicates an OSM POI, so the merged result keeps
        // exactly `count` POIs (all Overture). The assert inside the timed
        // closure guards against a future change that silently breaks the
        // dedupe.
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter_batched(
                || {
                    (
                        data_with_pois(count, FeatureSource::Osm),
                        data_with_overture_duplicates(count),
                    )
                },
                |(osm, overture)| {
                    let result = merge_source_data(osm, Some(overture), PoiSourceMode::Both);
                    assert_eq!(result.data.poi_nodes().len(), count);
                    assert_eq!(result.status, par_osm_rust::sources::SourceStatus::Both);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_build_pois_baseline(c: &mut Criterion) {
    // Baseline: measure OsmData-with-POIs construction alone (including the
    // clone-per-iteration in the merge bench), so the merge bench can be read
    // as "dedupe cost above construction". Not a perf target.
    let mut group = c.benchmark_group("build_pois_data");
    for &count in &[500usize, 2_000, 8_000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter(|| {
                let _ = data_with_pois(count, FeatureSource::Osm);
                let _ = data_with_overture_duplicates(count);
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_merge_source_data_dedupe,
    bench_build_pois_baseline
);
criterion_main!(benches);
