//! ARC-019: integration tests for the pure parse/write/merge/clip pipeline.
//!
//! All tests use ONLY the public API (`OsmData::default` + the `with_*`
//! builder, `parse_osm_xml_str`, `write_osm_xml_string`,
//! `merge_source_data`, `clip_to_bbox`, and the public read accessors on
//! `OsmData`). No network calls, no `blocking` feature, no overpass/
//! srtm/overture fetch paths — so the file compiles and passes under both
//! `cargo test --all-features` and `cargo test --no-default-features`.
//!
//! These complement the inline unit tests in `src/osm/mod.rs` (which can
//! access `pub(crate)` fields) by exercising the parser+writer+merger+clipper
//! together through the public surface a downstream consumer uses.

use std::collections::HashMap;
use std::path::Path;

use par_osm_rust::osm::{
    FeatureSource, OsmData, OsmNode, OsmPoiNode, OsmRelation, OsmWay, RelationMember,
    parse_osm_file, parse_osm_xml_str, parse_pbf, write_osm_xml_string,
};
use par_osm_rust::sources::{PoiSourceMode, SourceStatus, merge_source_data};

/// A synthetic OSM XML fixture exercising the parser's full structural surface
/// in a realistic order: bounds, plain nodes, a tagged POI node, a tagged
/// address node, a tagged tree node, a way with multiple tags, and a
/// multipolygon relation referencing that way. Used by the round-trip test.
const ROUND_TRIP_FIXTURE: &str = r#"<?xml version="1.0"?>
<osm version="0.6">
  <bounds minlat="51.5" minlon="-0.10" maxlat="51.51" maxlon="-0.09"/>
  <node id="1" lat="51.5" lon="-0.10"/>
  <node id="2" lat="51.5" lon="-0.09"/>
  <node id="3" lat="51.51" lon="-0.09"/>
  <node id="100" lat="51.505" lon="-0.095">
    <tag k="amenity" v="cafe"/>
    <tag k="name" v="A&amp;B Cafe"/>
  </node>
  <node id="101" lat="51.506" lon="-0.096">
    <tag k="addr:housenumber" v="42"/>
    <tag k="addr:street" v="Main St"/>
  </node>
  <node id="102" lat="51.507" lon="-0.094">
    <tag k="natural" v="tree"/>
  </node>
  <way id="10">
    <nd ref="1"/>
    <nd ref="2"/>
    <nd ref="3"/>
    <tag k="highway" v="residential"/>
    <tag k="name" v="Test Street"/>
  </way>
  <relation id="200">
    <member type="way" ref="10" role="outer"/>
    <tag k="type" v="multipolygon"/>
    <tag k="landuse" v="park"/>
  </relation>
</osm>"#;

/// Build an empty `OsmData` (no nodes/ways/POIs) via the public builder.
fn empty_osm_data() -> OsmData {
    OsmData::default()
}

/// Helper to build a tagged POI node.
fn poi(
    lat: f64,
    lon: f64,
    key: &str,
    value: &str,
    name: &str,
    source: FeatureSource,
) -> OsmPoiNode {
    let mut tags = HashMap::from([(key.to_string(), value.to_string())]);
    if !name.is_empty() {
        tags.insert("name".to_string(), name.to_string());
    }
    OsmPoiNode {
        lat,
        lon,
        tags,
        source,
    }
}

/// Extract the value of an XML attribute (`attr="..."`) from a single line.
fn xml_attr<'a>(line: &'a str, attr: &str) -> Option<&'a str> {
    let needle = format!("{attr}=\"");
    let start = line.find(&needle)? + needle.len();
    let end = line[start..].find('"')? + start;
    Some(&line[start..end])
}

/// Iterate `(lat, lon)` for every `<node ...>` line in an OSM XML string.
fn node_coords(xml: &str) -> Vec<(f64, f64)> {
    xml.lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("<node ") {
                return None;
            }
            let lat = xml_attr(trimmed, "lat")?.parse().ok()?;
            let lon = xml_attr(trimmed, "lon")?.parse().ok()?;
            Some((lat, lon))
        })
        .collect()
}

#[test]
fn parse_then_write_then_parse_preserves_full_osm_data() {
    // First parse: establishes the baseline structure from the inline fixture.
    let first = parse_osm_xml_str(ROUND_TRIP_FIXTURE).expect("first parse");
    first
        .validate_invariants()
        .expect("invariants after first parse");

    // Plain nodes registered (3 plain + 1 POI + 1 addr + 1 tree = 6).
    // ways: one residential way with three node refs and two tags.
    assert_eq!(first.iter_ways().count(), 1);
    let way = first.iter_ways().next().expect("one way");
    assert_eq!(way.id, 10);
    assert_eq!(way.node_refs, vec![1, 2, 3]);
    assert_eq!(
        way.tags.get("highway").map(String::as_str),
        Some("residential")
    );
    assert_eq!(
        way.tags.get("name").map(String::as_str),
        Some("Test Street")
    );

    // POI / address / tree node classification from tagged nodes.
    assert_eq!(first.poi_nodes().len(), 1);
    assert_eq!(
        first.poi_nodes()[0].tags.get("amenity").map(String::as_str),
        Some("cafe")
    );
    // XML entity decoding: `A&amp;B Cafe` → `A&B Cafe`.
    assert_eq!(
        first.poi_nodes()[0].tags.get("name").map(String::as_str),
        Some("A&B Cafe")
    );
    assert_eq!(first.poi_nodes()[0].source, FeatureSource::Osm);
    assert_eq!(first.addr_nodes().len(), 1);
    assert_eq!(
        first.addr_nodes()[0]
            .tags
            .get("addr:housenumber")
            .map(String::as_str),
        Some("42")
    );
    assert_eq!(first.tree_nodes().len(), 1);

    // Relation with way member + multipolygon tags.
    assert_eq!(first.relations().len(), 1);
    assert_eq!(
        first.relations()[0].tags.get("type").map(String::as_str),
        Some("multipolygon")
    );
    assert_eq!(
        first.relations()[0].tags.get("landuse").map(String::as_str),
        Some("park")
    );
    assert_eq!(first.relations()[0].members.len(), 1);
    assert_eq!(first.relations()[0].members[0].way_id, 10);
    assert_eq!(first.relations()[0].members[0].role, "outer");

    // Explicit <bounds> wins over node-derived bounds.
    assert_eq!(first.bounds(), Some((51.5, -0.10, 51.51, -0.09)));

    // Write to XML and re-parse: the round-trip must preserve every collection
    // that was observable on the first parse. This exercises the writer +
    // parser together — an area the audit flagged as untested at the
    // integration level.
    let written = write_osm_xml_string(&first);
    let second = parse_osm_xml_str(&written).expect("re-parse after write");
    second
        .validate_invariants()
        .expect("invariants after round-trip");

    // Way preserved (id, node_refs, tags).
    assert_eq!(second.iter_ways().count(), 1);
    let way2 = second.iter_ways().next().expect("one way after round-trip");
    assert_eq!(way2.id, 10);
    assert_eq!(way2.node_refs, vec![1, 2, 3]);
    assert_eq!(way2.tags, way.tags);

    // POI preserved with entity-decoded name round-tripping back through the
    // writer's escaping.
    assert_eq!(second.poi_nodes().len(), 1);
    assert_eq!(
        second.poi_nodes()[0]
            .tags
            .get("amenity")
            .map(String::as_str),
        Some("cafe")
    );
    assert_eq!(
        second.poi_nodes()[0].tags.get("name").map(String::as_str),
        Some("A&B Cafe")
    );

    // Address node preserved.
    assert_eq!(second.addr_nodes().len(), 1);
    assert_eq!(
        second.addr_nodes()[0]
            .tags
            .get("addr:housenumber")
            .map(String::as_str),
        Some("42")
    );

    // Tree node preserved (lat/lon only — no tags on OsmNode).
    assert_eq!(second.tree_nodes().len(), 1);

    // Relation preserved (tags, members, roles).
    assert_eq!(second.relations().len(), 1);
    assert_eq!(
        second.relations()[0].tags.get("type").map(String::as_str),
        Some("multipolygon")
    );
    assert_eq!(
        second.relations()[0]
            .tags
            .get("landuse")
            .map(String::as_str),
        Some("park")
    );
    assert_eq!(second.relations()[0].members.len(), 1);
    assert_eq!(second.relations()[0].members[0].way_id, 10);
    assert_eq!(second.relations()[0].members[0].role, "outer");

    // Bounds preserved.
    assert_eq!(second.bounds(), first.bounds());
}

#[test]
fn tagged_nodes_preserves_nodes_the_curated_collections_drop() {
    // ARC-004 + ENH-003: four standalone tagged nodes. Three are POIs the
    // curated `poi_nodes` collection keeps (`natural=peak`, `man_made=tower`,
    // `amenity=cafe`). The fourth — `natural=water` — is intentionally NOT
    // a POI under ENH-003's value-aware rules (only peak/rock/spring qualify),
    // not an address node, and not `natural=tree`, so every curated collection
    // drops it. `tagged_nodes` is the only place it survives; that preservation
    // is the load-bearing case this test name promises.
    const FIXTURE: &str = r#"<?xml version="1.0"?>
<osm version="0.6">
  <bounds minlat="51.5" minlon="-0.10" maxlat="51.51" maxlon="-0.09"/>
  <node id="1" lat="51.5" lon="-0.10"/>
  <node id="100" lat="51.505" lon="-0.095">
    <tag k="natural" v="peak"/>
    <tag k="name" v="Test Hill"/>
    <tag k="ele" v="123"/>
  </node>
  <node id="101" lat="51.506" lon="-0.096">
    <tag k="man_made" v="tower"/>
  </node>
  <node id="102" lat="51.507" lon="-0.094">
    <tag k="amenity" v="cafe"/>
    <tag k="name" v="Cafe"/>
  </node>
  <node id="103" lat="51.508" lon="-0.097">
    <tag k="natural" v="water"/>
    <tag k="name" v="Test Pond"/>
  </node>
</osm>"#;

    let data = parse_osm_xml_str(FIXTURE).expect("parse");

    // ENH-003: peak, tower, and cafe are POIs (value-aware rules); water is
    // value-filtered out of poi_nodes.
    assert_eq!(data.poi_nodes().len(), 3);
    let poi_has = |key: &str, value: &str| {
        data.poi_nodes()
            .iter()
            .any(|p| p.tags.get(key).map(String::as_str) == Some(value))
    };
    assert!(poi_has("natural", "peak"));
    assert!(poi_has("man_made", "tower"));
    assert!(poi_has("amenity", "cafe"));
    assert!(
        !poi_has("natural", "water"),
        "natural=water must NOT be in poi_nodes: {:?}",
        data.poi_nodes()
    );

    // `natural=water` is the genuinely-dropped node: no curated collection
    // claims it (not a POI, not addr:housenumber, not natural=tree), so
    // `tagged_nodes` is the only place it survives.
    assert!(data.addr_nodes().is_empty());
    assert!(data.tree_nodes().is_empty());

    // tagged_nodes is the lossless superset: all four tagged nodes.
    assert_eq!(data.tagged_nodes().len(), 4);
    let has = |key: &str, value: &str| {
        data.tagged_nodes()
            .iter()
            .any(|n| n.tags.get(key).map(String::as_str) == Some(value))
    };
    assert!(has("natural", "peak"));
    assert!(has("man_made", "tower"));
    assert!(has("amenity", "cafe"));
    assert!(has("natural", "water"));

    // The peak's non-classifying tags (`name`, `ele`) survive intact.
    let peak = data
        .tagged_nodes()
        .iter()
        .find(|n| n.tags.get("natural").map(String::as_str) == Some("peak"))
        .expect("peak in tagged_nodes");
    assert_eq!(peak.tags.get("name").map(String::as_str), Some("Test Hill"));
    assert_eq!(peak.tags.get("ele").map(String::as_str), Some("123"));

    // The water node's full tag map survives ONLY in tagged_nodes.
    let water = data
        .tagged_nodes()
        .iter()
        .find(|n| n.tags.get("natural").map(String::as_str) == Some("water"))
        .expect("water in tagged_nodes");
    assert_eq!(
        water.tags.get("name").map(String::as_str),
        Some("Test Pond")
    );

    // Round-trip through the writer must preserve peak/tower/water. Pre-0.3.1
    // the writer dropped them because they were never in a curated collection.
    let rewritten = parse_osm_xml_str(&write_osm_xml_string(&data)).expect("re-parse");
    assert_eq!(rewritten.tagged_nodes().len(), 4);
    assert!(
        rewritten
            .tagged_nodes()
            .iter()
            .any(|n| n.tags.get("natural").map(String::as_str) == Some("peak"))
    );
    assert!(
        rewritten
            .tagged_nodes()
            .iter()
            .any(|n| n.tags.get("man_made").map(String::as_str) == Some("tower"))
    );
    assert!(
        rewritten
            .tagged_nodes()
            .iter()
            .any(|n| n.tags.get("natural").map(String::as_str) == Some("water")),
        "round-trip must preserve the non-POI water node via tagged_nodes"
    );
    // The curated POIs are re-derived on re-parse.
    assert_eq!(rewritten.poi_nodes().len(), 3);
}

#[test]
fn merge_source_data_dedupes_duplicate_pois_preferring_overture() {
    // Two POIs at the same place (within the 25 m duplicate threshold) with
    // matching category + name. Overture must win under OverturePreferred.
    let osm = empty_osm_data().with_poi_nodes(vec![poi(
        51.50000,
        -0.10000,
        "amenity",
        "restaurant",
        "Diner",
        FeatureSource::Osm,
    )]);
    let overture = empty_osm_data().with_poi_nodes(vec![poi(
        51.50005,
        -0.10005,
        "amenity",
        "restaurant",
        "Diner",
        FeatureSource::Overture,
    )]);

    let merged = merge_source_data(osm, Some(overture), PoiSourceMode::OverturePreferred);

    assert_eq!(merged.status, SourceStatus::OverturePreferred);
    assert_eq!(merged.data.poi_nodes().len(), 1);
    assert_eq!(merged.data.poi_nodes()[0].source, FeatureSource::Overture);
    assert_eq!(
        merged.data.poi_nodes()[0]
            .tags
            .get("name")
            .map(String::as_str),
        Some("Diner")
    );
    assert!(merged.warnings.is_empty());
}

#[test]
fn merge_source_data_keeps_distinct_pois_under_both_mode() {
    // Same name but different POI category → not duplicates → both kept.
    let osm = empty_osm_data().with_poi_nodes(vec![poi(
        51.50000,
        -0.10000,
        "amenity",
        "restaurant",
        "Corner",
        FeatureSource::Osm,
    )]);
    let overture = empty_osm_data().with_poi_nodes(vec![poi(
        51.50005,
        -0.10005,
        "shop",
        "bakery",
        "Corner",
        FeatureSource::Overture,
    )]);

    let merged = merge_source_data(osm, Some(overture), PoiSourceMode::Both);

    assert_eq!(merged.status, SourceStatus::Both);
    assert_eq!(merged.data.poi_nodes().len(), 2);
    let sources: Vec<FeatureSource> = merged.data.poi_nodes().iter().map(|p| p.source).collect();
    assert!(sources.contains(&FeatureSource::Osm));
    assert!(sources.contains(&FeatureSource::Overture));
}

#[test]
fn merge_source_data_overture_preferred_falls_back_when_overture_absent() {
    let osm = empty_osm_data().with_poi_nodes(vec![poi(
        51.5,
        -0.1,
        "shop",
        "bakery",
        "Bakery",
        FeatureSource::Osm,
    )]);

    let merged = merge_source_data(osm, None, PoiSourceMode::OverturePreferred);

    assert_eq!(merged.status, SourceStatus::OvertureFallbackToOsm);
    assert_eq!(merged.data.poi_nodes().len(), 1);
    assert_eq!(merged.data.poi_nodes()[0].source, FeatureSource::Osm);
    assert!(
        merged
            .warnings
            .iter()
            .any(|w| w.contains("Overture POIs unavailable"))
    );
}

#[test]
fn clip_to_bbox_keeps_only_ways_touching_the_bbox_and_bounds_nodes() {
    // Build an 11×11 grid of nodes spanning lat/lon 0..=10. Two ways:
    //   - way 1: entirely inside the clip region (nodes at 4,4 / 4,6 / 6,6).
    //   - way 2: entirely outside the clip region (nodes at 8,8 / 8,10 / 10,10).
    // After clipping to (3.0, 3.0, 7.0, 7.0), only way 1 survives, bounds are
    // set to the clip bbox, and every surviving node falls inside the bbox.
    let nodes: HashMap<i64, OsmNode> = (0..=10i64)
        .flat_map(|i| {
            (0..=10i64).map(move |j| {
                (
                    i * 11 + j,
                    OsmNode {
                        lat: i as f64,
                        lon: j as f64,
                    },
                )
            })
        })
        .collect();
    let way_inside = OsmWay {
        id: 1,
        tags: HashMap::from([("highway".to_string(), "residential".to_string())]),
        node_refs: vec![4 * 11 + 4, 4 * 11 + 6, 6 * 11 + 6],
    };
    let way_outside = OsmWay {
        id: 2,
        tags: HashMap::from([("landuse".to_string(), "park".to_string())]),
        node_refs: vec![8 * 11 + 8, 8 * 11 + 10, 10 * 11 + 10],
    };
    let mut data = OsmData::default()
        .with_nodes(nodes)
        .with_ways(vec![way_inside, way_outside])
        // Attach a relation referencing the outside way; it must be pruned
        // because its only member way is outside the clip bbox.
        .with_relations(vec![OsmRelation {
            id: 999,
            tags: HashMap::from([("type".to_string(), "multipolygon".to_string())]),
            members: vec![RelationMember {
                way_id: 2,
                role: "outer".to_string(),
            }],
        }])
        .with_bounds(Some((0.0, 0.0, 10.0, 10.0)))
        // One POI inside the clip bbox and one outside.
        .with_poi_nodes(vec![
            poi(
                4.5,
                4.5,
                "amenity",
                "cafe",
                "Inside Cafe",
                FeatureSource::Osm,
            ),
            poi(
                9.0,
                9.0,
                "amenity",
                "cafe",
                "Outside Cafe",
                FeatureSource::Osm,
            ),
        ]);
    data.validate_invariants().expect("invariants before clip");

    let clip_bbox = (3.0, 3.0, 7.0, 7.0);
    data.clip_to_bbox(clip_bbox);
    data.validate_invariants().expect("invariants after clip");

    // Bounds reflect the clip bbox exactly.
    assert_eq!(data.bounds(), Some(clip_bbox));

    // Only the inside way survives; its tags survive too.
    let surviving: Vec<i64> = data.iter_ways().map(|w| w.id).collect();
    assert_eq!(surviving, vec![1]);
    let survived_way = data.iter_ways().next().expect("inside way survived");
    assert_eq!(
        survived_way.tags.get("highway").map(String::as_str),
        Some("residential")
    );

    // The relation referencing the outside way is pruned (its member no longer
    // exists in ways_by_id).
    assert!(
        data.relations().is_empty(),
        "relation with only-outside members must be pruned"
    );

    // Only the inside POI survives.
    assert_eq!(data.poi_nodes().len(), 1);
    assert_eq!(
        data.poi_nodes()[0].tags.get("name").map(String::as_str),
        Some("Inside Cafe")
    );

    // Every surviving node falls inside the clip bbox. `nodes` is pub(crate),
    // so verify via the serialized XML (the writer emits one `<node>` line per
    // surviving node with its lat/lon).
    let xml = write_osm_xml_string(&data);
    let (south, west, north, east) = clip_bbox;
    for (lat, lon) in node_coords(&xml) {
        assert!(
            lat >= south && lat <= north,
            "node lat {lat} outside clip bbox [{south}, {north}]"
        );
        assert!(
            lon >= west && lon <= east,
            "node lon {lon} outside clip bbox [{west}, {east}]"
        );
    }
}

#[test]
fn clip_to_bbox_on_empty_data_is_a_noop() {
    let mut data = empty_osm_data();
    data.clip_to_bbox((1.0, 1.0, 2.0, 2.0));
    data.validate_invariants()
        .expect("invariants after empty clip");
    assert_eq!(data.bounds(), Some((1.0, 1.0, 2.0, 2.0)));
    assert_eq!(data.iter_ways().count(), 0);
    assert!(data.poi_nodes().is_empty());
    assert!(data.relations().is_empty());
}

#[test]
fn merge_combines_ways_and_relations_from_both_sources() {
    // Two OsmData sources each contributing one distinct way + one relation.
    // After merge, both ways and both relations are present and the ways_by_id
    // invariant still holds (verifiable through the public validate_invariants
    // + way_id_at accessors).
    let osm = OsmData::default()
        .with_nodes(HashMap::from([(1, OsmNode { lat: 0.0, lon: 0.0 })]))
        .with_ways(vec![OsmWay {
            id: 100,
            tags: HashMap::new(),
            node_refs: vec![1],
        }])
        .with_relations(vec![OsmRelation {
            id: 1000,
            tags: HashMap::from([("type".to_string(), "multipolygon".to_string())]),
            members: vec![RelationMember {
                way_id: 100,
                role: "outer".to_string(),
            }],
        }])
        .with_bounds(Some((0.0, 0.0, 1.0, 1.0)));
    let other = OsmData::default()
        .with_nodes(HashMap::from([(2, OsmNode { lat: 1.0, lon: 1.0 })]))
        .with_ways(vec![OsmWay {
            id: 200,
            tags: HashMap::new(),
            node_refs: vec![2],
        }])
        .with_relations(vec![OsmRelation {
            id: 2000,
            tags: HashMap::from([("type".to_string(), "multipolygon".to_string())]),
            members: vec![RelationMember {
                way_id: 200,
                role: "outer".to_string(),
            }],
        }])
        .with_bounds(Some((1.0, 1.0, 2.0, 2.0)));

    let mut merged = osm;
    merged.merge(other);
    merged
        .validate_invariants()
        .expect("invariants after merge");

    // Both ways present, in insertion order, accessible via way_id_at.
    assert_eq!(merged.iter_ways().count(), 2);
    assert_eq!(merged.way_id_at(0), Some(100));
    assert_eq!(merged.way_id_at(1), Some(200));

    // Both relations present.
    assert_eq!(merged.relations().len(), 2);

    // Bounds unioned across the two sources.
    assert_eq!(merged.bounds(), Some((0.0, 0.0, 2.0, 2.0)));
}

// ────────────────────────────────────────────────────────────────────────────
// ENH-002: PBF format coverage. `parse_pbf` is an entire public input format
// that previously had zero test coverage — no `.pbf` fixture existed in the
// repo. The checked-in `tests/fixtures/pbf_parity.{osm,osm.pbf}` twins describe
// identical data, so the PBF parser's output can be pinned against the XML
// parser's across every collection. These tests also exercise the
// `parse_osm_file` dispatcher's `.osm.pbf` → `parse_pbf` branch, which was
// otherwise untested.
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn parse_pbf_matches_parse_osm_xml_for_equivalent_fixture() {
    // The .osm and .osm.pbf fixtures hold identical data; both parsers must
    // produce the same OsmData across every collection.
    let xml =
        parse_osm_file(Path::new("tests/fixtures/pbf_parity.osm")).expect("XML fixture parses");
    let pbf =
        parse_osm_file(Path::new("tests/fixtures/pbf_parity.osm.pbf")).expect("PBF fixture parses");
    xml.validate_invariants()
        .expect("XML fixture satisfies the ways/ways_by_id invariant");
    pbf.validate_invariants()
        .expect("PBF fixture satisfies the ways/ways_by_id invariant");

    // Collection counts agree across every collection.
    assert_eq!(xml.nodes().len(), pbf.nodes().len(), "node count");
    assert_eq!(xml.ways().len(), pbf.ways().len(), "way count");
    assert_eq!(xml.poi_nodes().len(), pbf.poi_nodes().len(), "poi count");
    assert_eq!(xml.addr_nodes().len(), pbf.addr_nodes().len(), "addr count");
    assert_eq!(xml.tree_nodes().len(), pbf.tree_nodes().len(), "tree count");
    assert_eq!(
        xml.tagged_nodes().len(),
        pbf.tagged_nodes().len(),
        "tagged count"
    );
    assert_eq!(
        xml.relations().len(),
        pbf.relations().len(),
        "relation count"
    );

    // Node map identical (id → (lat, lon)). OsmNode has no PartialEq, so
    // compare lat/lon with a tolerance tighter than PBF's nano-degree (1e-9)
    // encoding.
    assert_eq!(xml.nodes().len(), 11, "fixture has 11 nodes");
    for (id, exp) in xml.nodes() {
        let got = pbf
            .nodes()
            .get(id)
            .unwrap_or_else(|| panic!("node {id} present in XML but missing from PBF"));
        assert!(
            (got.lat - exp.lat).abs() < 1e-9,
            "node {id} lat differs: xml={} pbf={}",
            exp.lat,
            got.lat
        );
        assert!(
            (got.lon - exp.lon).abs() < 1e-9,
            "node {id} lon differs: xml={} pbf={}",
            exp.lon,
            got.lon
        );
    }

    // Ways: find each by id and compare tags + node_refs, robust to any
    // stream-order difference between the XML and PBF readers.
    for way_id in [10_i64, 20] {
        let x = xml
            .ways()
            .iter()
            .find(|w| w.id == way_id)
            .unwrap_or_else(|| panic!("way {way_id} present in XML"));
        let p = pbf
            .ways()
            .iter()
            .find(|w| w.id == way_id)
            .unwrap_or_else(|| panic!("way {way_id} present in PBF"));
        assert_eq!(x.node_refs, p.node_refs, "way {way_id} node_refs");
        assert_eq!(x.tags, p.tags, "way {way_id} tags");
    }
    // The closed building way preserves its first==last closing ref on both
    // paths (osmium does not dedupe it).
    let building = pbf
        .ways()
        .iter()
        .find(|w| w.id == 20)
        .expect("building way in PBF");
    assert_eq!(building.node_refs.first(), building.node_refs.last());
    assert_eq!(building.node_refs.len(), 4);

    // The cafe POI: identical tag map on both sides, including the UTF-8 name
    // ("Parity Café") — pins that PBF tag values round-trip as raw UTF-8.
    let xml_cafe = xml
        .poi_nodes()
        .iter()
        .find(|p| p.tags.get("amenity").map(String::as_str) == Some("cafe"))
        .expect("cafe POI missing from XML");
    let pbf_cafe = pbf
        .poi_nodes()
        .iter()
        .find(|p| p.tags.get("amenity").map(String::as_str) == Some("cafe"))
        .expect("cafe POI missing from PBF");
    assert_eq!(xml_cafe.tags, pbf_cafe.tags, "cafe tag map");
    assert_eq!(
        pbf_cafe.tags.get("name").map(String::as_str),
        Some("Parity Café"),
        "UTF-8 tag value must round-trip through PBF"
    );

    // Bounds: the fixture's explicit <bounds> equals the node lat/lon extrema,
    // so the XML parser (honors <bounds>) and the PBF parser (node-extrema
    // only — it does not read a PBF header bbox) agree. The XML side parses the
    // attribute via `f64::parse`; the PBF side decodes nano-degrees
    // (`coord_nanos / 1e9`). Those two float pipelines differ by ~1 ULP, so
    // compare componentwise with a tolerance tighter than PBF's 1e-9 precision
    // rather than asserting exact equality.
    let (xml_s, xml_w, xml_n, xml_e) = xml.bounds().expect("xml has bounds");
    let (pbf_s, pbf_w, pbf_n, pbf_e) = pbf.bounds().expect("pbf has bounds");
    for (axis, (x, p)) in [
        ("south", (xml_s, pbf_s)),
        ("west", (xml_w, pbf_w)),
        ("north", (xml_n, pbf_n)),
        ("east", (xml_e, pbf_e)),
    ] {
        assert!(
            (x - p).abs() < 1e-9,
            "bounds {axis} differs beyond 1e-9: xml={x} pbf={p}"
        );
    }
    // PBF bounds land on the fixture's intended bbox within tolerance.
    assert!((pbf_s - 51.500).abs() < 1e-9);
    assert!((pbf_w - -0.100).abs() < 1e-9);
    assert!((pbf_n - 51.515).abs() < 1e-9);
    assert!((pbf_e - -0.090).abs() < 1e-9);

    // Relation: id (ARC-113), tags, and member way_id/role all match.
    assert_eq!(xml.relations().len(), 1, "fixture has 1 relation");
    let xr = &xml.relations()[0];
    let pr = &pbf.relations()[0];
    assert_eq!(xr.id, pr.id, "relation id (ARC-113)");
    assert_eq!(xr.id, 200, "relation id preserved through PBF");
    assert_eq!(xr.tags, pr.tags, "relation tags");
    assert_eq!(xr.members.len(), pr.members.len(), "relation member count");
    assert_eq!(xr.members[0].way_id, pr.members[0].way_id, "member way_id");
    assert_eq!(xr.members[0].role, pr.members[0].role, "member role");
}

#[test]
fn parse_pbf_classifies_poi_address_tree_and_tagged_nodes() {
    // Pin the PBF-side classification directly (not just via XML parity).
    // ARC-105 will later consolidate the POI key list; this guards the PBF
    // path against drift.
    let pbf =
        parse_osm_file(Path::new("tests/fixtures/pbf_parity.osm.pbf")).expect("PBF fixture parses");

    // POI: amenity=cafe plus the two ENH-003 value-filtered POIs
    // (man_made=tower, natural=peak).
    assert_eq!(pbf.poi_nodes().len(), 3);
    assert!(
        pbf.poi_nodes()
            .iter()
            .any(|p| p.tags.get("amenity").map(String::as_str) == Some("cafe")),
        "cafe POI present: {:?}",
        pbf.poi_nodes()
    );
    assert!(
        pbf.poi_nodes()
            .iter()
            .all(|p| p.source == FeatureSource::Osm),
        "all PBF POIs must be OSM-sourced"
    );
    // man_made=pier is value-filtered OUT of poi_nodes (only tower/water_tower/
    // chimney qualify) — pins the negative case on the PBF path (ENH-003).
    assert!(
        pbf.poi_nodes()
            .iter()
            .all(|p| p.tags.get("man_made").map(String::as_str) != Some("pier")),
        "man_made=pier must NOT be a POI: {:?}",
        pbf.poi_nodes()
    );

    // Address: addr:housenumber classifies into addr_nodes.
    assert_eq!(pbf.addr_nodes().len(), 1);
    assert_eq!(
        pbf.addr_nodes()[0]
            .tags
            .get("addr:housenumber")
            .map(String::as_str),
        Some("42")
    );

    // Tree: natural=tree classifies into tree_nodes (lat/lon only, no tags).
    assert_eq!(pbf.tree_nodes().len(), 1);

    // ARC-004: every standalone tagged node lands in tagged_nodes — the
    // lossless superset. The fixture has six tagged nodes (3 POIs + address +
    // tree + the value-filtered man_made=pier).
    assert_eq!(pbf.tagged_nodes().len(), 6);
    assert!(
        pbf.tagged_nodes()
            .iter()
            .any(|n| n.tags.get("man_made").map(String::as_str) == Some("pier")),
        "man_made=pier survives in tagged_nodes: {:?}",
        pbf.tagged_nodes()
    );
}

#[test]
fn parse_pbf_returns_err_on_truncated_fixture() {
    // A truncated PBF must surface an Err, not panic. parse_pbf reads via
    // osmpbf's BufReader-backed ElementReader (not mmap), so a short blob
    // returns an I/O error propagated through the `?` in parse_pbf.
    use std::io::Write;
    let full = std::fs::read("tests/fixtures/pbf_parity.osm.pbf").expect("fixture file exists");
    assert!(
        full.len() > 40,
        "fixture must be larger than the truncation point"
    );
    let mut tmp = tempfile::Builder::new()
        .suffix(".osm.pbf")
        .tempfile()
        .expect("tempfile creation");
    tmp.write_all(&full[..40]).expect("write truncated fixture");
    tmp.flush().expect("flush truncated fixture");
    let (_, path) = tmp.into_parts();

    let result = parse_pbf(&path);
    assert!(
        result.is_err(),
        "truncated PBF must return Err, not Ok or a panic"
    );
}
