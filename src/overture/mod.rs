//! Overture Maps integration via the `overturemaps` Python CLI.
//!
//! This module provides helpers for checking whether the Overture CLI is
//! installed on the system PATH, invoking it to download GeoJSON data for a
//! given theme and bounding box, and converting the resulting GeoJSON into
//! the `OsmData` structure used by the rest of the pipeline.
//!
//! The `overturemaps` CLI (PyPI: `overturemaps`) is an optional runtime
//! dependency — callers should check [`is_cli_available`] before attempting
//! any download.  If the CLI is absent, the integration is silently skipped.
//!
//! Internally the module is split across four per-concern submodules whose
//! items are re-exported here so every original `crate::overture::*` path
//! keeps resolving (ARC-007 / QA-009):
//!
//! * `theme` — [`OvertureTheme`], [`ThemePriority`], and the Overture → OSM
//!   tag/category mapping.
//! * `parse` — [`parse_overture_geojson`] and its pure GeoJSON helpers.
//! * `cache` — [`OvertureParams`], the cache types/functions, and the
//!   on-disk GeoJSON cache I/O.
//! * `cli` — all `overturemaps` subprocess invocation and high-level fetch
//!   orchestration. The entire submodule is gated behind the `blocking`
//!   Cargo feature (ARC-012): `#[cfg(feature = "blocking")] mod cli;`.

mod cache;
mod parse;
mod theme;

#[cfg(feature = "blocking")]
mod cli;

pub use cache::{
    OvertureCacheEntry, OvertureCacheMeta, OvertureParams, clear_overture_cache,
    list_overture_areas, overture_cache_dir, overture_cache_key, overture_cache_key_with_version,
    overture_cache_read, overture_cache_write,
};
pub use parse::parse_overture_geojson;
// ARC-102: `ThemePriority` is deprecated (never implemented; will be removed
// in 0.3.0). The re-export stays through 0.3.0 so downstream code that
// references `crate::overture::ThemePriority` keeps compiling.
#[allow(deprecated)]
pub use theme::{OvertureTheme, ThemePriority};

#[cfg(feature = "blocking")]
pub use cli::{
    fetch_geojson_for_type, fetch_overture_data, fetch_overture_data_best_effort, is_cli_available,
};

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[cfg(feature = "blocking")]
    use super::cli::validate_cli_type;
    use super::*;
    use crate::osm::FeatureSource;
    use crate::synthetic_ids::OvertureIdAllocator;
    use crate::synthetic_ids::SYNTHETIC_OVERTURE_ID_BASE;
    use chrono::Utc;
    #[cfg(all(unix, feature = "blocking"))]
    use std::ffi::OsString;
    #[cfg(all(unix, feature = "blocking"))]
    use std::sync::Mutex;
    use std::time::Duration;
    #[cfg(all(unix, feature = "blocking"))]
    use std::time::Instant;

    // The PATH-mutation helpers below are only consumed by the
    // `#[cfg(all(unix, feature = "blocking"))]` subprocess test that installs a
    // fake `overturemaps` binary. Gate them the same way so they are not
    // flagged as dead code on Windows (where that test does not compile).
    #[cfg(all(unix, feature = "blocking"))]
    static PATH_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(all(unix, feature = "blocking"))]
    struct PathGuard {
        original_path: Option<OsString>,
    }

    #[cfg(all(unix, feature = "blocking"))]
    impl Drop for PathGuard {
        fn drop(&mut self) {
            match &self.original_path {
                // SAFETY (SEC-007): env mutation became `unsafe` in Rust 1.85
                // (Edition 2024) because it is not thread-safe across the
                // whole process. This test module serializes all such
                // mutations behind `PATH_LOCK` (a single Mutex held for the
                // duration of each test that touches PATH), so no other code
                // in this crate can read or write PATH concurrently. We do
                // not pull in `temp_env` because SEC-007 forbids editing
                // Cargo.toml in this wave. The original value is restored on
                // drop so the mutation is also scoped to the test.
                Some(path) => unsafe { std::env::set_var("PATH", path) },
                None => unsafe { std::env::remove_var("PATH") },
            }
        }
    }

    #[cfg(all(unix, feature = "blocking"))]
    fn prepend_to_path(path: &std::path::Path) -> PathGuard {
        let original_path = std::env::var_os("PATH");
        let mut paths = vec![path.to_path_buf()];
        if let Some(original) = &original_path {
            paths.extend(std::env::split_paths(original));
        }
        let joined = std::env::join_paths(paths).expect("join PATH entries");
        // SAFETY (SEC-007): see `PathGuard::drop` — caller holds `PATH_LOCK`
        // for the duration of this test, and the original value is restored
        // when the returned `PathGuard` drops.
        unsafe { std::env::set_var("PATH", joined) };

        PathGuard { original_path }
    }

    #[cfg(all(unix, feature = "blocking"))]
    fn write_fake_overturemaps(dir: &std::path::Path, script: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("overturemaps");
        std::fs::write(&path, script).expect("write fake overturemaps script");
        let mut permissions = std::fs::metadata(&path)
            .expect("fake overturemaps metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("chmod fake overturemaps script");
        path
    }

    // ── helpers ──────────────────────────────────────────────────────────

    fn point_feature(lon: f64, lat: f64, props: serde_json::Value) -> String {
        serde_json::json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": {
                    "type": "Point",
                    "coordinates": [lon, lat]
                },
                "properties": props
            }]
        })
        .to_string()
    }

    fn polygon_feature(props: serde_json::Value) -> String {
        // A simple 4-corner square polygon.
        serde_json::json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [[
                        [0.0, 0.0],
                        [0.0, 1.0],
                        [1.0, 1.0],
                        [1.0, 0.0],
                        [0.0, 0.0]
                    ]]
                },
                "properties": props
            }]
        })
        .to_string()
    }

    fn line_feature(props: serde_json::Value) -> String {
        serde_json::json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": {
                    "type": "LineString",
                    "coordinates": [
                        [0.0, 0.0],
                        [0.0, 1.0],
                        [1.0, 1.0]
                    ]
                },
                "properties": props
            }]
        })
        .to_string()
    }

    // ── Theme parsing tests ──────────────────────────────────────────────

    #[test]
    fn from_str_loose_parses_address_singular_and_plural() {
        assert_eq!(
            OvertureTheme::from_str_loose("address"),
            Some(OvertureTheme::Address)
        );
        assert_eq!(
            OvertureTheme::from_str_loose("addresses"),
            Some(OvertureTheme::Address)
        );
    }

    #[test]
    fn from_str_loose_preserves_existing_accepted_forms() {
        assert_eq!(
            OvertureTheme::from_str_loose("buildings"),
            Some(OvertureTheme::Building)
        );
        assert_eq!(
            OvertureTheme::from_str_loose("roads"),
            Some(OvertureTheme::Transportation)
        );
        assert_eq!(
            OvertureTheme::from_str_loose("landuse"),
            Some(OvertureTheme::Base)
        );
        assert_eq!(
            OvertureTheme::from_str_loose("addr"),
            Some(OvertureTheme::Address)
        );
    }

    // ── CLI tests ────────────────────────────────────────────────────────

    #[cfg(all(unix, feature = "blocking"))]
    #[test]
    fn fetch_geojson_drains_large_stderr_without_waiting_for_timeout() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        write_fake_overturemaps(
            tmp.path(),
            r#"#!/bin/sh
	printf 'fake overturemaps useful error: stderr flood begins\n' >&2
	i=0
	while [ "$i" -lt 20000 ]; do
	  printf 'stderr filler line %05d abcdefghijklmnopqrstuvwxyz\n' "$i" >&2
	  i=$((i + 1))
	done
	printf 'fake overturemaps useful error: final diagnostic\n' >&2
	exit 23
"#,
        );

        let _lock = PATH_LOCK.lock().expect("PATH lock poisoned");
        let _path_guard = prepend_to_path(tmp.path());
        let start = Instant::now();

        let err = fetch_geojson_for_type("place", (51.5, -0.13, 51.52, -0.10), 5)
            .expect_err("fake CLI should fail");

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "fetch should return promptly instead of waiting for timeout; elapsed {:?}",
            start.elapsed()
        );
        let message = err.to_string();
        assert!(
            message.contains("fake overturemaps useful error"),
            "error should include useful stderr snippet, got: {message}"
        );
    }

    // ── Building tests ───────────────────────────────────────────────────

    #[test]
    fn building_with_class_height_floors() {
        let geojson = polygon_feature(serde_json::json!({
            "class": "residential",
            "height": 12.5,
            "num_floors": 4
        }));
        let data = parse_overture_geojson(&geojson, OvertureTheme::Building).unwrap();
        assert_eq!(data.ways.len(), 1);
        let tags = &data.ways[0].tags;
        assert_eq!(tags["building"], "residential");
        assert_eq!(tags["building:height"], "12.5");
        assert_eq!(tags["building:levels"], "4");
    }

    #[test]
    fn building_no_class_defaults_yes() {
        let geojson = polygon_feature(serde_json::json!({}));
        let data = parse_overture_geojson(&geojson, OvertureTheme::Building).unwrap();
        assert_eq!(data.ways.len(), 1);
        assert_eq!(data.ways[0].tags["building"], "yes");
    }

    // ── Transportation tests ─────────────────────────────────────────────

    #[test]
    fn transportation_all_fields() {
        let geojson = line_feature(serde_json::json!({
            "class": "primary",
            "names": { "primary": "Main Street" },
            "road_surface": "paved",
            "is_bridge": true,
            "is_tunnel": false
        }));
        let data = parse_overture_geojson(&geojson, OvertureTheme::Transportation).unwrap();
        assert_eq!(data.ways.len(), 1);
        let tags = &data.ways[0].tags;
        assert_eq!(tags["highway"], "primary");
        assert_eq!(tags["name"], "Main Street");
        assert_eq!(tags["surface"], "paved");
        assert_eq!(tags["bridge"], "yes");
        assert!(!tags.contains_key("tunnel"));
    }

    #[test]
    fn transportation_no_class_defaults_unclassified() {
        let geojson = line_feature(serde_json::json!({}));
        let data = parse_overture_geojson(&geojson, OvertureTheme::Transportation).unwrap();
        assert_eq!(data.ways[0].tags["highway"], "unclassified");
    }

    // ── Place tests ──────────────────────────────────────────────────────

    #[test]
    fn place_becomes_poi_node() {
        let geojson = point_feature(
            -0.1,
            51.5,
            serde_json::json!({
                "categories": { "primary": "restaurant" },
                "names": { "primary": "The Bistro" }
            }),
        );
        let data = parse_overture_geojson(&geojson, OvertureTheme::Place).unwrap();
        assert_eq!(data.poi_nodes.len(), 1);
        assert_eq!(data.poi_nodes[0].tags["amenity"], "restaurant");
        assert_eq!(data.poi_nodes[0].tags["name"], "The Bistro");
        assert_eq!(data.poi_nodes[0].source, FeatureSource::Overture);
        assert!((data.poi_nodes[0].lat - 51.5).abs() < 1e-9);
        assert!((data.poi_nodes[0].lon - -0.1).abs() < 1e-9);
    }

    // ── Base theme tests ─────────────────────────────────────────────────

    #[test]
    fn base_water_subtype_maps_to_natural_water() {
        let geojson = polygon_feature(serde_json::json!({
            "subtype": "lake",
            "class": "lake"
        }));
        let data = parse_overture_geojson(&geojson, OvertureTheme::Base).unwrap();
        assert_eq!(data.ways.len(), 1);
        assert_eq!(data.ways[0].tags["natural"], "water");
        assert_eq!(data.ways[0].tags["water"], "lake");
    }

    #[test]
    fn base_landuse_forest_subtype() {
        let geojson = polygon_feature(serde_json::json!({
            "subtype": "forest",
            "class": "forest"
        }));
        let data = parse_overture_geojson(&geojson, OvertureTheme::Base).unwrap();
        assert_eq!(data.ways.len(), 1);
        assert_eq!(data.ways[0].tags["landuse"], "forest");
    }

    // ── Address tests ────────────────────────────────────────────────────

    #[test]
    fn address_becomes_addr_node() {
        let geojson = point_feature(
            -0.2,
            51.6,
            serde_json::json!({
                "number": "42",
                "street": "Baker Street"
            }),
        );
        let data = parse_overture_geojson(&geojson, OvertureTheme::Address).unwrap();
        assert_eq!(data.addr_nodes.len(), 1);
        assert_eq!(data.addr_nodes[0].tags["addr:housenumber"], "42");
        assert_eq!(data.addr_nodes[0].tags["addr:street"], "Baker Street");
        assert_eq!(data.addr_nodes[0].source, FeatureSource::Overture);
        // Should NOT appear in poi_nodes.
        assert_eq!(data.poi_nodes.len(), 0);
    }

    // ── Edge cases ───────────────────────────────────────────────────────

    #[test]
    fn empty_feature_collection_returns_empty_osm_data() {
        let geojson = r#"{"type":"FeatureCollection","features":[]}"#;
        let data = parse_overture_geojson(geojson, OvertureTheme::Building).unwrap();
        assert!(data.nodes.is_empty());
        assert!(data.ways.is_empty());
        assert!(data.poi_nodes.is_empty());
        assert!(data.addr_nodes.is_empty());
        assert!(data.bounds.is_none());
    }

    #[test]
    fn multipolygon_produces_multiple_ways() {
        let geojson = serde_json::json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": {
                    "type": "MultiPolygon",
                    "coordinates": [
                        [[[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [0.0, 0.0]]],
                        [[[2.0, 2.0], [2.0, 3.0], [3.0, 3.0], [2.0, 2.0]]]
                    ]
                },
                "properties": { "class": "office" }
            }]
        })
        .to_string();
        let data = parse_overture_geojson(&geojson, OvertureTheme::Building).unwrap();
        assert_eq!(data.ways.len(), 2);
    }

    #[test]
    fn bounds_computed_correctly() {
        let geojson = polygon_feature(serde_json::json!({}));
        let data = parse_overture_geojson(&geojson, OvertureTheme::Building).unwrap();
        let (min_lat, min_lon, max_lat, max_lon) = data.bounds.unwrap();
        assert!((min_lat - 0.0).abs() < 1e-9);
        assert!((min_lon - 0.0).abs() < 1e-9);
        assert!((max_lat - 1.0).abs() < 1e-9);
        assert!((max_lon - 1.0).abs() < 1e-9);
    }

    // ── Determinism tests (ARC-009 / QA-010) ────────────────────────────

    #[test]
    fn parse_overture_geojson_is_deterministic_across_calls() {
        // Two parses of identical GeoJSON must produce identical synthetic
        // IDs (the per-parse allocator resets on each call). The previous
        // global AtomicI64 design made the second parse's IDs depend on the
        // first.
        let geojson = polygon_feature(serde_json::json!({}));
        let first = parse_overture_geojson(&geojson, OvertureTheme::Building).unwrap();
        let second = parse_overture_geojson(&geojson, OvertureTheme::Building).unwrap();

        let first_way_id = first.way_id_at(0).expect("first parse has a way");
        let second_way_id = second.way_id_at(0).expect("second parse has a way");
        assert_eq!(
            first_way_id, second_way_id,
            "way IDs diverged across identical parses"
        );
        assert_eq!(first.nodes.len(), second.nodes.len());
        assert_eq!(
            first
                .nodes
                .keys()
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            second
                .nodes
                .keys()
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            "node IDs diverged across identical parses"
        );
    }

    // ── Multi-theme merge tests (ARC-101) ───────────────────────────────

    #[test]
    fn shared_allocator_keeps_multi_theme_merge_consistent() {
        // Regression for ARC-101: two themes parsed via ONE shared
        // allocator must produce disjoint way IDs, so merging the second
        // into the first keeps `ways`/`ways_by_id` consistent and every
        // original node coordinate survives.
        let building_geojson = polygon_feature(serde_json::json!({"class": "residential"}));
        let segment_geojson = line_feature(serde_json::json!({"class": "primary"}));

        let mut alloc = OvertureIdAllocator::new();
        let mut building = super::parse::parse_overture_geojson_with_allocator(
            &building_geojson,
            OvertureTheme::Building,
            &mut alloc,
        )
        .unwrap();
        let segment = super::parse::parse_overture_geojson_with_allocator(
            &segment_geojson,
            OvertureTheme::Transportation,
            &mut alloc,
        )
        .unwrap();

        // Snapshot the building node coordinates + way count before merge.
        let building_way_count = building.ways.len();
        let segment_way_count = segment.ways.len();
        let building_node_coords_before: Vec<(f64, f64)> =
            building.nodes.values().map(|n| (n.lat, n.lon)).collect();
        let segment_node_coords: Vec<(f64, f64)> =
            segment.nodes.values().map(|n| (n.lat, n.lon)).collect();

        building.merge(segment);

        // (a) invariant survives the merge.
        assert!(
            building.validate_invariants().is_ok(),
            "merge produced an inconsistent OsmData after a shared-allocator multi-theme parse"
        );
        // (b) way count is the sum of both parses.
        assert_eq!(
            building.ways.len(),
            building_way_count + segment_way_count,
            "way count after merge must equal the sum of both parses"
        );
        // (c) every pre-merge node coordinate is still present (no
        // last-write-wins overwrite of a building node by a segment node).
        for coord in &building_node_coords_before {
            assert!(
                building.nodes.values().any(|n| (n.lat, n.lon) == *coord),
                "building node coordinate {coord:?} disappeared after merge"
            );
        }
        for coord in &segment_node_coords {
            assert!(
                building.nodes.values().any(|n| (n.lat, n.lon) == *coord),
                "segment node coordinate {coord:?} disappeared after merge"
            );
        }
    }

    #[test]
    fn independent_allocators_collide_on_first_id() {
        // Pins the ARC-101 rationale: two independent allocators in the
        // same band DO collide — they both start at SYNTHETIC_OVERTURE_ID_BASE.
        // This is exactly why fetch orchestration owns one allocator per
        // fetch instead of letting each per-theme parse construct its own.
        let mut a = OvertureIdAllocator::new();
        let mut b = OvertureIdAllocator::new();
        let a_first = a.next_id();
        let b_first = b.next_id();
        assert_eq!(
            a_first, b_first,
            "two fresh allocators must collide on their first ID (both start at SYNTHETIC_OVERTURE_ID_BASE)"
        );
        assert_eq!(a_first, SYNTHETIC_OVERTURE_ID_BASE);
    }

    // ── Cache tests ──────────────────────────────────────────────────────

    #[test]
    fn overture_cache_key_is_deterministic() {
        let bbox = (51.5, -0.13, 51.52, -0.10);
        let k1 = overture_cache_key(bbox, "building");
        let k2 = overture_cache_key(bbox, "building");
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64, "SHA-256 hex should be 64 chars");
    }

    #[test]
    fn overture_cache_key_varies_by_theme() {
        let bbox = (51.5, -0.13, 51.52, -0.10);
        let k1 = overture_cache_key(bbox, "building");
        let k2 = overture_cache_key(bbox, "segment");
        assert_ne!(k1, k2);
    }

    #[test]
    fn overture_cache_key_with_version_is_deterministic() {
        let bbox = (51.5, -0.13, 51.52, -0.10);
        let k1 = overture_cache_key_with_version(bbox, "building", "0.4.0");
        let k2 = overture_cache_key_with_version(bbox, "building", "0.4.0");
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64, "SHA-256 hex should be 64 chars");
    }

    #[test]
    fn overture_cache_key_with_version_varies_by_cli_version() {
        // ARC-001: a CLI upgrade must invalidate the cache.
        let bbox = (51.5, -0.13, 51.52, -0.10);
        let k_old = overture_cache_key_with_version(bbox, "building", "0.4.0");
        let k_new = overture_cache_key_with_version(bbox, "building", "0.5.0");
        assert_ne!(k_old, k_new);
    }

    #[test]
    fn overture_cache_key_with_version_differs_from_legacy_key() {
        // ARC-001: the new v2 canonical form must not collide with v1.
        let bbox = (51.5, -0.13, 51.52, -0.10);
        let legacy = overture_cache_key(bbox, "building");
        let versioned = overture_cache_key_with_version(bbox, "building", "0.4.0");
        assert_ne!(legacy, versioned);
    }

    #[test]
    fn overture_cache_write_read_roundtrip() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let bbox = (51.5_f64, -0.13_f64, 51.52_f64, -0.10_f64);
        let key = overture_cache_key_with_version(bbox, "building", "test");
        let geojson = r#"{"type":"FeatureCollection","features":[]}"#;

        overture_cache_write(tmp.path(), &key, bbox, "building", "test", geojson).unwrap();
        // `None` disables TTL enforcement.
        let result = overture_cache_read(tmp.path(), &key, None);
        assert_eq!(result.as_deref(), Some(geojson));
    }

    #[test]
    fn overture_cache_read_returns_none_when_ttl_exceeded() {
        // ARC-001: an entry older than the TTL is treated as a miss.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let bbox = (51.5_f64, -0.13_f64, 51.52_f64, -0.10_f64);
        let key = overture_cache_key_with_version(bbox, "building", "test");
        let geojson = r#"{"type":"FeatureCollection","features":[]}"#;

        // Hand-write a meta file whose `created_at` is well in the past so
        // any positive TTL is exceeded.
        let meta_path = tmp.path().join(format!("{key}.meta.json"));
        let geojson_path = tmp.path().join(format!("{key}.geojson"));
        std::fs::write(&geojson_path, geojson).unwrap();
        let past = Utc::now() - chrono::Duration::days(365);
        let meta = serde_json::json!({
            "bbox": [bbox.0, bbox.1, bbox.2, bbox.3],
            "cli_type": "building",
            "created_at": past,
            "size_bytes": geojson.len() as u64,
            "cli_version": "test",
        });
        std::fs::write(&meta_path, meta.to_string()).unwrap();

        // 1-second TTL — entry is a year old, so this must miss.
        let result = overture_cache_read(tmp.path(), &key, Some(Duration::from_secs(1)));
        assert!(
            result.is_none(),
            "expired entry should be treated as a miss"
        );
    }

    #[test]
    fn overture_cache_read_returns_data_when_entry_is_fresh() {
        // ARC-001 counterpart: a freshly-written entry is a hit.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let bbox = (51.5_f64, -0.13_f64, 51.52_f64, -0.10_f64);
        let key = overture_cache_key_with_version(bbox, "building", "test");
        let geojson = r#"{"type":"FeatureCollection","features":[]}"#;

        overture_cache_write(tmp.path(), &key, bbox, "building", "test", geojson).unwrap();
        // 30-day TTL — entry is seconds old, so this must hit.
        let result = overture_cache_read(
            tmp.path(),
            &key,
            Some(Duration::from_secs(30 * 24 * 60 * 60)),
        );
        assert_eq!(result.as_deref(), Some(geojson));
    }

    #[test]
    fn overture_cache_read_returns_none_when_meta_missing_under_ttl() {
        // ARC-001: when TTL is set but the meta file is absent/unreadable,
        // we cannot enforce freshness — treat as a miss rather than serve
        // potentially stale data without a timestamp.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let bbox = (51.5_f64, -0.13_f64, 51.52_f64, -0.10_f64);
        let key = overture_cache_key_with_version(bbox, "building", "test");
        let geojson_path = tmp.path().join(format!("{key}.geojson"));
        std::fs::write(
            &geojson_path,
            r#"{"type":"FeatureCollection","features":[]}"#,
        )
        .unwrap();

        let result = overture_cache_read(tmp.path(), &key, Some(Duration::from_secs(60)));
        assert!(result.is_none(), "missing meta under TTL must miss");
    }

    // ── SEC-012 argument-injection guard ────────────────────────────────

    #[cfg(feature = "blocking")]
    #[test]
    fn validate_cli_type_rejects_dash_and_whitespace() {
        // Bare theme name accepted.
        assert!(validate_cli_type("building").is_ok());
        assert!(validate_cli_type("land_use").is_ok()); // underscore, not dash

        // Empty rejected.
        assert!(validate_cli_type("").is_err());

        // Argument-injection shapes rejected (would let the value be parsed
        // as a CLI flag by overturemaps).
        assert!(validate_cli_type("--output=/etc/passwd").is_err());
        assert!(validate_cli_type("-t").is_err());
        assert!(validate_cli_type("building segment").is_err());
        assert!(validate_cli_type("building\tsegment").is_err());
        assert!(validate_cli_type("\nbuilding").is_err());
    }

    #[cfg(feature = "blocking")]
    #[test]
    fn fetch_geojson_for_type_rejects_argument_injection() {
        // SEC-012: a user-controlled cli_type must not reach the CLI as a flag.
        let err = fetch_geojson_for_type("--output=/tmp/evil", (0.0, 0.0, 1.0, 1.0), 1)
            .expect_err("dashed cli_type must be rejected before spawn");
        let msg = err.to_string();
        assert!(
            msg.contains("SEC-012") || msg.contains("argument-injection"),
            "error should mention the SEC-012 guard, got: {msg}"
        );
    }
}
