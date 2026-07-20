//! OpenStreetMap data model, parsers, and serializer.
//!
//! This module owns the [`OsmData`] model shared across the crate (nodes,
//! ways, multipolygon relations, POI / address / tree node collections, and
//! the dataset bounding box) plus the functions that produce and consume it:
//!
//! * [`parse_pbf`] reads a `.osm.pbf` file via `osmpbf`'s memory-mapped
//!   `ElementReader`.
//! * [`parse_osm_xml_str`] / [`parse_osm_xml`] / [`parse_osm_xml_file`] /
//!   [`parse_osm_file`] parse OSM XML in a single-pass event loop that
//!   handles nodes, ways, and relations in whatever order they appear
//!   (Overpass does not guarantee node-before-way ordering).
//!   [`parse_osm_xml_file`] streams from disk via a `BufReader` so peak
//!   memory stays bounded on large `.osm` extracts (ARC-013).
//! * [`write_osm_xml_string`] serializes an [`OsmData`] back into the simple
//!   OSM XML dialect this crate and `osm-world` can re-parse.
//!
//! [`OsmData`] also exposes [`OsmData::merge`] (combine two datasets) and
//! [`OsmData::clip_to_bbox`] (intersect with a bounding box), used by the
//! upstream fetch orchestration in [`crate::sources`].
//!
//! Internally the module is split across four per-concern submodules whose
//! items are re-exported here so every original `crate::osm::*` path keeps
//! resolving (ARC-007 / QA-009):
//!
//! * `model` — the data-model structs/enums + [`OsmData`]'s inherent impls.
//! * `pbf` — [`parse_pbf`] and its per-node helper.
//! * `xml_parse` — [`parse_osm_xml_str`], [`parse_osm_xml_file`], the
//!   file-format dispatcher, and the QA-005 attribute helpers.
//! * `xml_write` — [`write_osm_xml_string`] and its tag/attr helpers.

mod model;
mod pbf;
mod xml_parse;
mod xml_write;

pub use model::{FeatureSource, OsmData, OsmNode, OsmPoiNode, OsmRelation, OsmWay, RelationMember};
pub use pbf::parse_pbf;
pub use xml_parse::{parse_osm_file, parse_osm_xml, parse_osm_xml_file, parse_osm_xml_str};
pub use xml_write::write_osm_xml_string;

pub(crate) use model::POI_TAG_KEYS;

#[cfg(test)]
// ARC-109 (0.3.0): the lib's own tests exercise the deprecated `OsmData::new`
// constructor for coverage of the legacy positional-argument path. Migrating
// every call site to the builder would obscure what's under test, so the
// module allows `deprecated` at the module granularity.
#[allow(deprecated)]
mod tests {
    use super::*;
    use crate::synthetic_ids::SYNTHETIC_NODE_ID_BASE;
    use std::collections::HashMap;

    const MINIMAL_OSM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<osm version="0.6">
  <node id="1" lat="51.5" lon="-0.10"/>
  <node id="2" lat="51.5" lon="-0.09"/>
  <node id="3" lat="51.51" lon="-0.09"/>
  <way id="10">
    <nd ref="1"/>
    <nd ref="2"/>
    <nd ref="3"/>
    <tag k="highway" v="residential"/>
    <tag k="name" v="Test Street"/>
  </way>
</osm>"#;

    #[test]
    fn parse_xml_nodes() {
        let data = parse_osm_xml_str(MINIMAL_OSM).unwrap();
        assert_eq!(data.nodes.len(), 3);
        let n = data.nodes.get(&1).unwrap();
        assert!((n.lat - 51.5).abs() < 0.0001);
        assert!((n.lon - -0.10).abs() < 0.0001);
    }

    #[test]
    fn parse_xml_ways() {
        let data = parse_osm_xml_str(MINIMAL_OSM).unwrap();
        assert_eq!(data.ways.len(), 1);
        assert_eq!(data.ways[0].id, 10);
        assert_eq!(data.ways[0].tags["highway"], "residential");
        assert_eq!(data.ways[0].tags["name"], "Test Street");
        assert_eq!(data.ways[0].node_refs, vec![1, 2, 3]);
    }

    #[test]
    fn parse_xml_bounds_computed_from_nodes() {
        let data = parse_osm_xml_str(MINIMAL_OSM).unwrap();
        let (min_lat, min_lon, max_lat, max_lon) = data.bounds.unwrap();
        assert!((min_lat - 51.5).abs() < 0.0001);
        assert!((max_lat - 51.51).abs() < 0.0001);
        assert!((min_lon - -0.10).abs() < 0.0001);
        assert!((max_lon - -0.09).abs() < 0.0001);
    }

    #[test]
    fn parse_xml_preserves_explicit_bounds() {
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <bounds minlat="10.0" minlon="20.0" maxlat="30.0" maxlon="40.0"/>
  <node id="1" lat="11.0" lon="21.0"/>
</osm>"#;

        let data = parse_osm_xml_str(xml).unwrap();

        assert_eq!(data.bounds, Some((10.0, 20.0, 30.0, 40.0)));
    }

    #[test]
    fn parse_xml_nodes_after_ways() {
        // Overpass does not guarantee node-before-way ordering
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <way id="1">
    <nd ref="10"/>
    <nd ref="11"/>
    <tag k="highway" v="primary"/>
  </way>
  <node id="10" lat="1.0" lon="1.0"/>
  <node id="11" lat="1.1" lon="1.1"/>
</osm>"#;
        let data = parse_osm_xml_str(xml).unwrap();
        assert_eq!(data.nodes.len(), 2);
        assert_eq!(data.ways.len(), 1);
        assert_eq!(data.ways[0].node_refs, vec![10, 11]);
    }

    #[test]
    fn parse_xml_multipolygon_relation() {
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1" lat="0.0" lon="0.0"/>
  <way id="100">
    <nd ref="1"/>
    <tag k="landuse" v="park"/>
  </way>
  <relation id="200">
    <member type="way" ref="100" role="outer"/>
    <tag k="type" v="multipolygon"/>
    <tag k="landuse" v="park"/>
  </relation>
</osm>"#;
        let data = parse_osm_xml_str(xml).unwrap();
        assert_eq!(data.relations.len(), 1);
        // ARC-113: parser now populates `OsmRelation::id` from the
        // `<relation id="…">` attribute.
        assert_eq!(data.relations[0].id, 200);
        assert_eq!(data.relations[0].members[0].way_id, 100);
        assert_eq!(data.relations[0].members[0].role, "outer");
        assert_eq!(data.relations[0].tags["landuse"], "park");
    }

    #[test]
    fn parse_xml_skips_relation_with_missing_id() {
        // ARC-113: a relation without an `id` attribute is skipped with a
        // warning (mirroring QA-101's way-id policy) rather than defaulted
        // to 0 and colliding with other id-less relations.
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1" lat="0.0" lon="0.0"/>
  <way id="100">
    <nd ref="1"/>
  </way>
  <relation>
    <member type="way" ref="100" role="outer"/>
    <tag k="type" v="multipolygon"/>
  </relation>
</osm>"#;
        let data = parse_osm_xml_str(xml).unwrap();
        assert!(
            data.relations.is_empty(),
            "id-less relation must be skipped (ARC-113)"
        );
    }

    #[test]
    fn parse_xml_skips_duplicate_relation_id() {
        // ARC-113: first-wins on duplicate relation ids in the same document
        // (mirrors the way-id policy).
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1" lat="0.0" lon="0.0"/>
  <way id="100">
    <nd ref="1"/>
  </way>
  <relation id="500">
    <member type="way" ref="100" role="outer"/>
    <tag k="type" v="multipolygon"/>
    <tag k="name" v="first"/>
  </relation>
  <relation id="500">
    <member type="way" ref="100" role="outer"/>
    <tag k="type" v="multipolygon"/>
    <tag k="name" v="second"/>
  </relation>
</osm>"#;
        let data = parse_osm_xml_str(xml).unwrap();
        assert_eq!(data.relations.len(), 1);
        assert_eq!(data.relations[0].id, 500);
        assert_eq!(data.relations[0].tags["name"], "first");
    }

    #[test]
    fn relation_id_survives_parse_write_parse_round_trip() {
        // ARC-113 round-trip: the relation's OSM id must survive the
        // parse→write→parse cycle (writer emits the real id when present,
        // parser re-reads it from the emitted XML).
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1" lat="0.0" lon="0.0"/>
  <way id="100">
    <nd ref="1"/>
  </way>
  <relation id="123456789">
    <member type="way" ref="100" role="outer"/>
    <tag k="type" v="multipolygon"/>
    <tag k="landuse" v="park"/>
  </relation>
</osm>"#;
        let first = parse_osm_xml_str(xml).unwrap();
        assert_eq!(first.relations[0].id, 123456789);

        let written = write_osm_xml_string(&first);
        assert!(
            written.contains("<relation id=\"123456789\">"),
            "writer must emit the real relation id: {written}"
        );

        let second = parse_osm_xml_str(&written).unwrap();
        assert_eq!(second.relations.len(), 1);
        assert_eq!(second.relations[0].id, 123456789);
        assert_eq!(second.relations[0].tags["landuse"], "park");
    }

    #[test]
    fn parse_xml_unescapes_node_way_and_relation_tag_attributes() {
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1" lat="0.0" lon="0.0">
    <tag k="amenity" v="cafe"/>
    <tag k="name" v="A&amp;B"/>
    <tag k="brand&amp;operator" v="C&amp;D"/>
  </node>
  <way id="100">
    <nd ref="1"/>
    <tag k="highway" v="residential"/>
    <tag k="name&amp;operator" v="A&amp;B Road"/>
  </way>
  <relation id="200">
    <member type="way" ref="100" role="outer&amp;ring"/>
    <tag k="type" v="multipolygon"/>
    <tag k="landuse&amp;name" v="A&amp;B Park"/>
  </relation>
</osm>"#;

        let data = parse_osm_xml_str(xml).unwrap();

        assert_eq!(data.poi_nodes[0].tags["name"], "A&B");
        assert_eq!(data.poi_nodes[0].tags["brand&operator"], "C&D");
        assert_eq!(data.ways[0].tags["name&operator"], "A&B Road");
        assert_eq!(data.relations[0].members[0].role, "outer&ring");
        assert_eq!(data.relations[0].tags["landuse&name"], "A&B Park");
    }

    #[test]
    fn parse_xml_poi_nodes_collected() {
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1" lat="51.5" lon="-0.10"/>
  <node id="2" lat="51.51" lon="-0.11">
    <tag k="amenity" v="restaurant"/>
    <tag k="name" v="The Pub"/>
  </node>
  <node id="3" lat="51.52" lon="-0.12">
    <tag k="shop" v="supermarket"/>
  </node>
</osm>"#;
        let data = parse_osm_xml_str(xml).unwrap();
        assert_eq!(data.nodes.len(), 3);
        assert_eq!(data.poi_nodes.len(), 2);
        assert_eq!(data.poi_nodes[0].tags["amenity"], "restaurant");
        assert_eq!(data.poi_nodes[0].tags["name"], "The Pub");
        assert_eq!(data.poi_nodes[1].tags["shop"], "supermarket");
    }

    #[test]
    fn parse_xml_poi_nodes_are_marked_osm_source() {
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1" lat="51.5" lon="-0.1">
    <tag k="amenity" v="restaurant"/>
    <tag k="name" v="The Pub"/>
  </node>
</osm>"#;

        let data = parse_osm_xml_str(xml).unwrap();

        assert_eq!(data.poi_nodes.len(), 1);
        assert_eq!(data.poi_nodes[0].source, FeatureSource::Osm);
    }

    #[test]
    fn parse_xml_address_nodes_are_marked_osm_source() {
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1" lat="51.5" lon="-0.1">
    <tag k="addr:housenumber" v="42"/>
    <tag k="addr:street" v="Baker Street"/>
  </node>
</osm>"#;

        let data = parse_osm_xml_str(xml).unwrap();

        assert_eq!(data.addr_nodes.len(), 1);
        assert_eq!(data.addr_nodes[0].source, FeatureSource::Osm);
    }

    #[test]
    fn parse_xml_non_poi_nodes_not_collected() {
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1" lat="51.5" lon="-0.10"/>
  <node id="2" lat="51.51" lon="-0.11">
    <tag k="natural" v="tree"/>
  </node>
</osm>"#;
        let data = parse_osm_xml_str(xml).unwrap();
        assert_eq!(data.poi_nodes.len(), 0);
    }

    #[test]
    fn parse_osm_file_detects_format() {
        use std::io::Write;
        let mut tmp = tempfile::Builder::new().suffix(".osm").tempfile().unwrap();
        tmp.write_all(MINIMAL_OSM.as_bytes()).unwrap();
        let (_, path) = tmp.into_parts();
        let data = parse_osm_file(&path).unwrap();
        assert_eq!(data.nodes.len(), 3);
    }

    #[test]
    fn write_osm_xml_string_serializes_poi_nodes_with_tags() {
        let data = OsmData::new(
            HashMap::new(),
            Vec::new(),
            Vec::new(),
            Some((51.5, -0.1, 51.6, -0.0)),
            vec![OsmPoiNode {
                lat: 51.55,
                lon: -0.05,
                tags: HashMap::from([
                    ("amenity".to_string(), "restaurant".to_string()),
                    ("name".to_string(), "A&B Cafe".to_string()),
                ]),
                source: FeatureSource::Overture,
            }],
            Vec::new(),
            Vec::new(),
        );

        let xml = write_osm_xml_string(&data);

        assert!(
            xml.contains("<bounds minlat=\"51.5\" minlon=\"-0.1\" maxlat=\"51.6\" maxlon=\"-0\"/>")
        );
        assert!(xml.contains("<tag k=\"amenity\" v=\"restaurant\"/>"));
        assert!(xml.contains("<tag k=\"name\" v=\"A&amp;B Cafe\"/>"));
    }

    #[test]
    fn write_osm_xml_string_round_trips_poi_nodes_through_parser() {
        let data = OsmData::new(
            HashMap::new(),
            Vec::new(),
            Vec::new(),
            Some((51.5, -0.1, 51.6, -0.0)),
            vec![OsmPoiNode {
                lat: 51.55,
                lon: -0.05,
                tags: HashMap::from([("shop".to_string(), "bakery".to_string())]),
                source: FeatureSource::Overture,
            }],
            Vec::new(),
            Vec::new(),
        );

        let xml = write_osm_xml_string(&data);
        let parsed = parse_osm_xml_str(&xml).unwrap();

        assert_eq!(parsed.poi_nodes.len(), 1);
        assert_eq!(
            parsed.poi_nodes[0].tags.get("shop").map(String::as_str),
            Some("bakery")
        );
    }

    #[test]
    fn write_osm_xml_string_round_trips_relations_with_tags_and_members() {
        let data = OsmData::new(
            HashMap::from([
                (1, OsmNode { lat: 0.0, lon: 0.0 }),
                (2, OsmNode { lat: 1.0, lon: 1.0 }),
            ]),
            vec![
                OsmWay {
                    id: 100,
                    tags: HashMap::from([("landuse".to_string(), "park".to_string())]),
                    node_refs: vec![1, 2],
                },
                OsmWay {
                    id: 101,
                    tags: HashMap::from([("natural".to_string(), "water".to_string())]),
                    node_refs: vec![2, 1],
                },
            ],
            vec![OsmRelation {
                id: 500,
                tags: HashMap::from([
                    ("type".to_string(), "multipolygon".to_string()),
                    ("name".to_string(), "A&B Park".to_string()),
                ]),
                members: vec![
                    RelationMember {
                        way_id: 100,
                        role: "outer".to_string(),
                    },
                    RelationMember {
                        way_id: 101,
                        role: "inner".to_string(),
                    },
                ],
            }],
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        let xml = write_osm_xml_string(&data);
        // ARC-113: the writer now emits the relation's own OSM id (`500`)
        // rather than the synthetic `writer_relation_id(0)` fallback.
        assert!(xml.contains("<relation id=\"500\">"));

        let parsed = parse_osm_xml_str(&xml).unwrap();

        assert_eq!(parsed.relations.len(), 1);
        assert_eq!(parsed.relations[0].id, 500);
        assert_eq!(parsed.relations[0].tags["name"], "A&B Park");
        assert_eq!(parsed.relations[0].tags["type"], "multipolygon");
        assert_eq!(parsed.relations[0].members.len(), 2);
        assert_eq!(parsed.relations[0].members[0].way_id, 100);
        assert_eq!(parsed.relations[0].members[0].role, "outer");
        assert_eq!(parsed.relations[0].members[1].way_id, 101);
        assert_eq!(parsed.relations[0].members[1].role, "inner");
    }

    #[test]
    fn write_osm_xml_string_allocates_synthetic_node_ids_without_collisions() {
        let data = OsmData::new(
            HashMap::from([(SYNTHETIC_NODE_ID_BASE, OsmNode { lat: 0.0, lon: 0.0 })]),
            Vec::new(),
            Vec::new(),
            None,
            vec![OsmPoiNode {
                lat: 1.0,
                lon: 1.0,
                tags: HashMap::from([("amenity".to_string(), "cafe".to_string())]),
                source: FeatureSource::Overture,
            }],
            Vec::new(),
            Vec::new(),
        );

        let first_xml = write_osm_xml_string(&data);
        assert!(first_xml.contains("<node id=\"-9000000001\""));

        let parsed = parse_osm_xml_str(&first_xml).unwrap();
        let second_xml = write_osm_xml_string(&parsed);

        let node_ids: Vec<i64> = second_xml
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix("<node id=\""))
            .filter_map(|rest| rest.split_once('"'))
            .map(|(id, _)| id.parse::<i64>().unwrap())
            .collect();
        let unique_node_ids: std::collections::HashSet<_> = node_ids.iter().copied().collect();

        assert_eq!(node_ids.len(), unique_node_ids.len());
        assert!(unique_node_ids.contains(&-9_000_000_002));
    }

    #[test]
    fn new_seeds_ways_by_id_from_way_ids() {
        let data = OsmData::new(
            HashMap::from([
                (1, OsmNode { lat: 0.0, lon: 0.0 }),
                (2, OsmNode { lat: 1.0, lon: 1.0 }),
            ]),
            vec![
                OsmWay {
                    id: 100,
                    tags: HashMap::new(),
                    node_refs: vec![1, 2],
                },
                OsmWay {
                    id: 101,
                    tags: HashMap::new(),
                    node_refs: vec![2, 1],
                },
            ],
            Vec::new(),
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        // Invariant holds by construction.
        data.validate_invariants().unwrap();
        assert_eq!(data.ways.len(), 2);
        assert_eq!(data.ways_by_id.get(&100), Some(&0));
        assert_eq!(data.ways_by_id.get(&101), Some(&1));
        assert_eq!(data.way_id_at(0), Some(100));
        assert_eq!(data.way_id_at(1), Some(101));
        assert_eq!(data.way_id_at(2), None);
        assert_eq!(data.iter_ways().count(), 2);
    }

    #[test]
    fn push_way_appends_to_both_ways_and_ways_by_id() {
        let mut data = OsmData::new(
            HashMap::new(),
            Vec::new(),
            Vec::new(),
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        data.push_way(OsmWay {
            id: 7,
            tags: HashMap::new(),
            node_refs: vec![],
        });
        data.push_way(OsmWay {
            id: 9,
            tags: HashMap::new(),
            node_refs: vec![],
        });

        data.validate_invariants().unwrap();
        assert_eq!(data.ways.len(), 2);
        assert_eq!(data.ways_by_id.get(&7), Some(&0));
        assert_eq!(data.ways_by_id.get(&9), Some(&1));
        assert_eq!(data.way_id_at(0), Some(7));
        assert_eq!(data.way_id_at(1), Some(9));
    }

    #[test]
    fn validate_invariants_detects_a_drift_between_ways_and_ways_by_id() {
        // Construct a consistent OsmData, then deliberately break the
        // invariant to confirm validate_invariants surfaces the drift.
        let mut data = OsmData::new(
            HashMap::new(),
            vec![OsmWay {
                id: 1,
                tags: HashMap::new(),
                node_refs: vec![],
            }],
            Vec::new(),
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        assert!(data.validate_invariants().is_ok());

        // Push a way into `ways` without updating `ways_by_id` — simulates the
        // exact drift ARC-008 was filed against.
        data.ways.push(OsmWay {
            id: 2,
            tags: HashMap::new(),
            node_refs: vec![],
        });
        let err = data.validate_invariants().expect_err("must detect drift");
        assert!(err.contains("ways_by_id length"), "unexpected error: {err}");
    }

    #[test]
    fn validate_invariants_detects_a_drift_between_ways_id_and_ways_by_id() {
        // QA-021: with `ways[].id` now the source of truth, validate_invariants
        // must also catch a `ways_by_id` that disagrees with `ways[].id` even
        // when the lengths and per-index uniqueness still line up.
        let mut data = OsmData::new(
            HashMap::new(),
            vec![OsmWay {
                id: 1,
                tags: HashMap::new(),
                node_refs: vec![],
            }],
            Vec::new(),
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        assert!(data.validate_invariants().is_ok());

        // Rewire ways_by_id to point at a different id for index 0.
        data.ways_by_id.clear();
        data.ways_by_id.insert(999, 0);
        let err = data
            .validate_invariants()
            .expect_err("must detect id drift");
        assert!(
            err.contains("ways[0].id") && err.contains("missing from ways_by_id"),
            "unexpected error: {err}"
        );
    }

    // ── ARC-103: merge skip-on-collision + invariant guard ──────────────

    /// Build an [`OsmData`] with a single way carrying `way_id` and one node.
    fn one_way_osm(way_id: i64, node_id: i64, lat: f64, lon: f64) -> OsmData {
        OsmData::new(
            HashMap::from([(node_id, OsmNode { lat, lon })]),
            vec![OsmWay {
                id: way_id,
                tags: HashMap::from([("name".to_string(), format!("way-{way_id}"))]),
                node_refs: vec![node_id],
            }],
            Vec::new(),
            Some((lat, lon, lat, lon)),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    }

    #[test]
    fn merge_keeps_first_way_on_collision_and_preserves_invariant() {
        // ARC-103: a way ID present on both sides must NOT corrupt the
        // `ways` / `ways_by_id` invariant. Skip-first-wins: the first way
        // (in `self`) survives, the duplicate (in `other`) is dropped with
        // a warning. The trailing debug_assert! in `merge` must not fire.
        let mut a = one_way_osm(10, 1, 51.50, -0.10);
        let b = one_way_osm(10, 2, 48.85, 2.35); // same way id, different node/coord

        a.merge(b);

        // The first way (id=10, name="way-10", node=1) is retained.
        assert_eq!(
            a.ways.len(),
            1,
            "colliding way must be skipped, not appended"
        );
        assert_eq!(a.ways[0].id, 10);
        assert_eq!(a.ways[0].tags["name"], "way-10");
        // Its node survives.
        assert_eq!(
            a.nodes.len(),
            2,
            "both nodes (1 and 2) are kept; nodes are last-write-wins by id, and the ids differ"
        );
        assert!(a.nodes.contains_key(&1));
        assert!(a.nodes.contains_key(&2));

        // The invariant holds — and the debug_assert! at the end of `merge`
        // did not fire (otherwise this test would have panicked in debug
        // builds before reaching the assertion).
        assert!(a.validate_invariants().is_ok());
    }

    #[test]
    fn merge_appends_disjoint_ways_and_preserves_invariant() {
        // ARC-103 positive case: two disjoint way-ID sets merge cleanly.
        let mut a = one_way_osm(10, 1, 51.50, -0.10);
        let b = one_way_osm(20, 2, 48.85, 2.35);

        a.merge(b);

        assert_eq!(a.ways.len(), 2);
        assert_eq!(a.ways[0].id, 10);
        assert_eq!(a.ways[1].id, 20);
        assert!(a.validate_invariants().is_ok());
    }

    #[test]
    fn parse_osm_xml_str_returns_data_that_satisfies_the_invariant() {
        let data = parse_osm_xml_str(MINIMAL_OSM).unwrap();
        data.validate_invariants().unwrap();
    }

    // ────────────────────────────────────────────────────────────────────
    // ARC-006 test-first: pin the parser's current observable behavior
    // before the single-pass rewrite, then keep these tests green after.
    // ────────────────────────────────────────────────────────────────────

    /// One document exercising every structural feature the parser must
    /// handle: explicit `<bounds>`, out-of-order relation -> way -> node,
    /// POI / address / tree tagged nodes, way tags + `<nd ref>` members,
    /// and relation `<member>` of node/way/relation types (non-way filtered).
    const COMPREHENSIVE_OSM: &str = r#"<?xml version="1.0"?>
<osm version="0.6">
  <bounds minlat="10.0" minlon="20.0" maxlat="30.0" maxlon="40.0"/>
  <relation id="500">
    <member type="way" ref="200" role="outer"/>
    <member type="node" ref="100" role="label"/>
    <member type="relation" ref="900" role="sub"/>
    <member type="way" ref="201" role="inner"/>
    <tag k="type" v="multipolygon"/>
    <tag k="landuse" v="park"/>
    <tag k="name" v="A&amp;B Park"/>
  </relation>
  <way id="200">
    <nd ref="100"/>
    <nd ref="101"/>
    <tag k="highway" v="residential"/>
    <tag k="name" v="First &amp; Main"/>
  </way>
  <way id="201">
    <nd ref="101"/>
    <nd ref="102"/>
  </way>
  <node id="100" lat="11.0" lon="21.0"/>
  <node id="101" lat="12.0" lon="22.0"/>
  <node id="102" lat="13.0" lon="23.0">
    <tag k="amenity" v="cafe"/>
    <tag k="name" v="A&amp;B Cafe"/>
  </node>
  <node id="103" lat="14.0" lon="24.0">
    <tag k="addr:housenumber" v="42"/>
    <tag k="addr:street" v="Main St"/>
  </node>
  <node id="104" lat="15.0" lon="25.0">
    <tag k="natural" v="tree"/>
  </node>
</osm>"#;

    #[test]
    fn parse_xml_comprehensive_fixture_pins_full_parser_behavior() {
        let data = parse_osm_xml_str(COMPREHENSIVE_OSM).unwrap();
        data.validate_invariants().unwrap();

        // Explicit <bounds> wins over node-derived bounds.
        assert_eq!(data.bounds, Some((10.0, 20.0, 30.0, 40.0)));

        // All five nodes registered (three standalone + two tagged).
        assert_eq!(data.nodes.len(), 5);
        assert_eq!(
            data.nodes.get(&100).map(|n| (n.lat, n.lon)),
            Some((11.0, 21.0))
        );

        // Out-of-order ways resolved by node-id reference (no position map needed).
        assert_eq!(data.ways.len(), 2);
        assert_eq!(data.way_id_at(0), Some(200));
        assert_eq!(data.way_id_at(1), Some(201));
        assert_eq!(data.ways[0].id, 200);
        assert_eq!(data.ways[1].id, 201);
        assert_eq!(data.ways[0].node_refs, vec![100, 101]);
        assert_eq!(data.ways[0].tags["highway"], "residential");
        assert_eq!(data.ways[0].tags["name"], "First & Main");
        assert!(data.ways[1].tags.is_empty());
        assert_eq!(data.ways[1].node_refs, vec![101, 102]);

        // Relation keeps only `type="way"` members, in order.
        assert_eq!(data.relations.len(), 1);
        let rel = &data.relations[0];
        assert_eq!(rel.members.len(), 2);
        assert_eq!(rel.members[0].way_id, 200);
        assert_eq!(rel.members[0].role, "outer");
        assert_eq!(rel.members[1].way_id, 201);
        assert_eq!(rel.members[1].role, "inner");
        assert_eq!(rel.tags["type"], "multipolygon");
        assert_eq!(rel.tags["name"], "A&B Park");

        // POI / address / tree classification from tagged nodes.
        assert_eq!(data.poi_nodes.len(), 1);
        assert_eq!(data.poi_nodes[0].tags["name"], "A&B Cafe");
        assert_eq!(data.poi_nodes[0].source, FeatureSource::Osm);
        assert_eq!(data.addr_nodes.len(), 1);
        assert_eq!(data.addr_nodes[0].tags["addr:housenumber"], "42");
        assert_eq!(data.addr_nodes[0].source, FeatureSource::Osm);
        assert_eq!(data.tree_nodes.len(), 1);
        assert_eq!(data.tree_nodes[0].lat, 15.0);
    }

    #[test]
    fn parse_xml_tree_node_collected_into_tree_nodes() {
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1" lat="1.5" lon="2.5">
    <tag k="natural" v="tree"/>
  </node>
</osm>"#;

        let data = parse_osm_xml_str(xml).unwrap();
        // A `natural=tree` node is NOT classified as a POI.
        assert_eq!(data.poi_nodes.len(), 0);
        assert_eq!(data.tree_nodes.len(), 1);
        assert!((data.tree_nodes[0].lat - 1.5).abs() < f64::EPSILON);
        assert!((data.tree_nodes[0].lon - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_xml_classifies_value_filtered_poi_keys() {
        // ENH-003: the shared `is_poi` predicate value-filters `man_made`
        // (tower/water_tower/chimney) and `natural` (peak/rock/spring). The
        // five cases below pin every arm of the table — `natural=tree` is
        // NOT a POI and continues to route to `tree_nodes`.

        // man_made=tower → in poi_nodes with the full tag map retained.
        let data = parse_osm_xml_str(
            r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1001" lat="51.501" lon="-0.091">
    <tag k="man_made" v="tower"/>
    <tag k="name" v="Tower A"/>
  </node>
</osm>"#,
        )
        .unwrap();
        assert_eq!(
            data.poi_nodes.len(),
            1,
            "man_made=tower must land in poi_nodes"
        );
        assert_eq!(
            data.poi_nodes[0].tags.get("man_made").map(String::as_str),
            Some("tower")
        );
        assert_eq!(
            data.poi_nodes[0].tags.get("name").map(String::as_str),
            Some("Tower A"),
            "tag map must be retained on the POI entry"
        );
        assert!(
            data.nodes.contains_key(&1001),
            "tagged node must also appear in the plain nodes map"
        );

        // man_made=pier → value-filtered out of poi_nodes.
        let data = parse_osm_xml_str(
            r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1002" lat="51.502" lon="-0.092">
    <tag k="man_made" v="pier"/>
  </node>
</osm>"#,
        )
        .unwrap();
        assert!(
            data.poi_nodes.is_empty(),
            "man_made=pier must NOT be a POI: {:?}",
            data.poi_nodes
        );

        // natural=peak → in poi_nodes.
        let data = parse_osm_xml_str(
            r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1003" lat="51.503" lon="-0.093">
    <tag k="natural" v="peak"/>
    <tag k="name" v="Hilltop"/>
  </node>
</osm>"#,
        )
        .unwrap();
        assert_eq!(
            data.poi_nodes.len(),
            1,
            "natural=peak must land in poi_nodes"
        );
        assert_eq!(
            data.poi_nodes[0].tags.get("natural").map(String::as_str),
            Some("peak")
        );
        assert_eq!(
            data.poi_nodes[0].tags.get("name").map(String::as_str),
            Some("Hilltop")
        );

        // natural=tree → in tree_nodes, NOT in poi_nodes.
        let data = parse_osm_xml_str(
            r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1004" lat="51.504" lon="-0.094">
    <tag k="natural" v="tree"/>
  </node>
</osm>"#,
        )
        .unwrap();
        assert!(
            data.poi_nodes.is_empty(),
            "natural=tree must NOT be in poi_nodes: {:?}",
            data.poi_nodes
        );
        assert_eq!(
            data.tree_nodes.len(),
            1,
            "natural=tree must land in tree_nodes"
        );
        assert!((data.tree_nodes[0].lat - 51.504).abs() < 1e-9);
        assert!((data.tree_nodes[0].lon - -0.094).abs() < 1e-9);

        // natural=water → value-filtered out of both poi_nodes and tree_nodes.
        let data = parse_osm_xml_str(
            r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1005" lat="51.505" lon="-0.095">
    <tag k="natural" v="water"/>
  </node>
</osm>"#,
        )
        .unwrap();
        assert!(
            data.poi_nodes.is_empty(),
            "natural=water must NOT be in poi_nodes: {:?}",
            data.poi_nodes
        );
        assert!(
            data.tree_nodes.is_empty(),
            "natural=water must NOT be in tree_nodes: {:?}",
            data.tree_nodes
        );
    }

    #[test]
    fn parse_xml_drops_relation_when_all_members_are_non_way() {
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <relation id="1">
    <member type="node" ref="10" role="label"/>
    <member type="relation" ref="20" role="subarea"/>
    <tag k="type" v="multipolygon"/>
  </relation>
</osm>"#;

        let data = parse_osm_xml_str(xml).unwrap();

        // All members are non-way, so the multipolygon has zero members and
        // is dropped entirely (matches the parser's existing invariant).
        assert_eq!(data.relations.len(), 0);
    }

    #[test]
    fn parse_xml_accepts_start_form_of_bounds_element() {
        // Some emitters write `<bounds ...></bounds>` instead of `<bounds .../>`.
        // Both forms must set explicit_bounds identically.
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <bounds minlat="1.0" minlon="2.0" maxlat="3.0" maxlon="4.0"></bounds>
  <node id="1" lat="10.0" lon="20.0"/>
</osm>"#;

        let data = parse_osm_xml_str(xml).unwrap();

        assert_eq!(data.bounds, Some((1.0, 2.0, 3.0, 4.0)));
    }

    #[test]
    fn parse_xml_rejects_excessively_nested_elements() {
        // SEC-004: element nesting is capped to bound stack growth from a
        // malicious payload. Legitimate OSM XML is ~3 deep; 100 nested
        // unknown elements must trip the limit.
        let opens: String = "<a>".repeat(100);
        let closes: String = "</a>".repeat(100);
        let xml = format!("<?xml version=\"1.0\"?>\n<osm>{opens}{closes}</osm>");

        let result = parse_osm_xml_str(&xml);
        let err = match result {
            Err(e) => e.to_string(),
            Ok(_) => panic!("depth limit must reject deeply nested XML"),
        };
        assert!(
            err.contains("depth") || err.contains("nesting"),
            "expected a depth/nesting error, got: {err}"
        );
    }

    #[test]
    fn write_then_parse_round_trip_preserves_full_osm_data() {
        // Round-trip a representative OsmData through write_osm_xml_string
        // and parse_osm_xml_str. Every collection must come back equal.
        let original = OsmData::new(
            HashMap::from([
                (
                    1,
                    OsmNode {
                        lat: 51.5,
                        lon: -0.10,
                    },
                ),
                (
                    2,
                    OsmNode {
                        lat: 51.5,
                        lon: -0.09,
                    },
                ),
                (
                    3,
                    OsmNode {
                        lat: 51.51,
                        lon: -0.09,
                    },
                ),
            ]),
            vec![OsmWay {
                id: 10,
                tags: HashMap::from([
                    ("highway".to_string(), "residential".to_string()),
                    ("name".to_string(), "Test Street".to_string()),
                ]),
                node_refs: vec![1, 2, 3],
            }],
            vec![OsmRelation {
                id: 200,
                tags: HashMap::from([
                    ("type".to_string(), "multipolygon".to_string()),
                    ("landuse".to_string(), "park".to_string()),
                ]),
                members: vec![RelationMember {
                    way_id: 10,
                    role: "outer".to_string(),
                }],
            }],
            Some((51.5, -0.10, 51.51, -0.09)),
            vec![OsmPoiNode {
                lat: 51.505,
                lon: -0.095,
                tags: HashMap::from([("amenity".to_string(), "cafe".to_string())]),
                source: FeatureSource::Osm,
            }],
            Vec::new(),
            Vec::new(),
        );

        let xml = write_osm_xml_string(&original);
        let parsed = parse_osm_xml_str(&xml).unwrap();
        parsed.validate_invariants().unwrap();

        // The writer mints one synthetic POI node id, so the parsed node set
        // is the original three plus one.
        assert_eq!(parsed.nodes.len(), original.nodes.len() + 1);
        for (id, node) in &original.nodes {
            let got = parsed.nodes.get(id).expect("node round-tripped");
            assert!((got.lat - node.lat).abs() < 1e-9);
            assert!((got.lon - node.lon).abs() < 1e-9);
        }
        assert_eq!(parsed.ways.len(), original.ways.len());
        assert_eq!(parsed.way_id_at(0), original.way_id_at(0));
        assert_eq!(parsed.ways[0].node_refs, original.ways[0].node_refs);
        assert_eq!(parsed.ways[0].tags, original.ways[0].tags);
        assert_eq!(parsed.relations.len(), original.relations.len());
        assert_eq!(
            parsed.relations[0].members.len(),
            original.relations[0].members.len()
        );
        assert_eq!(parsed.relations[0].tags, original.relations[0].tags);
        assert_eq!(parsed.poi_nodes.len(), original.poi_nodes.len());
        assert_eq!(parsed.poi_nodes[0].tags, original.poi_nodes[0].tags);
        assert_eq!(parsed.bounds, original.bounds);
    }

    #[test]
    fn write_osm_xml_string_skips_dangling_nd_refs() {
        // ARC-016: a way that references a node id not present in `nodes`
        // must not emit a dangling `<nd ref>` — the serialized XML must be
        // structurally valid (every <nd ref> resolves to a real <node>).
        let data = OsmData::new(
            HashMap::from([
                (1, OsmNode { lat: 0.0, lon: 0.0 }),
                (2, OsmNode { lat: 1.0, lon: 1.0 }),
            ]),
            vec![OsmWay {
                id: 10,
                tags: HashMap::new(),
                // Node 999 is dangling — not present in `nodes`.
                node_refs: vec![1, 2, 999],
            }],
            Vec::new(),
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        let xml = write_osm_xml_string(&data);

        // Present refs are still emitted; the dangling one is skipped.
        assert!(xml.contains("<nd ref=\"1\"/>"));
        assert!(xml.contains("<nd ref=\"2\"/>"));
        assert!(
            !xml.contains("<nd ref=\"999\"/>"),
            "dangling <nd ref> must not appear in output: {xml}"
        );

        // The dangling-ref-free XML must round-trip through the parser
        // without error, and the surviving way keeps its non-dangling refs.
        let parsed = parse_osm_xml_str(&xml).expect("dangling-ref-free XML must reparse");
        parsed.validate_invariants().unwrap();
        assert_eq!(parsed.way_id_at(0), Some(10));
        assert_eq!(parsed.ways[0].node_refs, vec![1, 2]);
    }

    #[test]
    fn write_osm_xml_string_way_id_lookup_is_o1_correct() {
        // ARC-003 / QA-001 / QA-021: the writer reads `way.id` directly, so
        // each way block must carry the id stored on the struct at that index.
        // Two ways are exercised to confirm the writer is not silently
        // reusing a single id or falling back to a synthetic.
        let data = OsmData::new(
            HashMap::from([
                (1, OsmNode { lat: 0.0, lon: 0.0 }),
                (2, OsmNode { lat: 1.0, lon: 1.0 }),
                (3, OsmNode { lat: 2.0, lon: 2.0 }),
                (4, OsmNode { lat: 3.0, lon: 3.0 }),
            ]),
            vec![
                OsmWay {
                    id: 100,
                    tags: HashMap::new(),
                    node_refs: vec![1, 2],
                },
                OsmWay {
                    id: 200,
                    tags: HashMap::new(),
                    node_refs: vec![3, 4],
                },
            ],
            Vec::new(),
            None,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        let xml = write_osm_xml_string(&data);

        // Each way block carries the id stored on the way struct at that
        // index — the QA-021 writer contract.
        let way_lines: Vec<&str> = xml
            .lines()
            .filter(|line| line.trim_start().starts_with("<way id=\""))
            .collect();
        assert_eq!(way_lines.len(), 2);
        assert!(way_lines[0].contains("id=\"100\""));
        assert!(way_lines[1].contains("id=\"200\""));
    }

    // ────────────────────────────────────────────────────────────────────
    // ARC-013: streaming parse_osm_xml_file must produce output identical
    // to parse_osm_xml_str(&read_to_string(path)), and must surface a clear
    // error on a missing input file. OsmData does not derive PartialEq, so
    // equivalence is checked field-by-field (matching the round-trip test
    // pattern above).
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn parse_osm_xml_file_equivalence_matches_parse_osm_xml_str() {
        use std::io::Write;
        let mut tmp = tempfile::Builder::new()
            .suffix(".osm")
            .tempfile()
            .expect("tempfile creation");
        tmp.write_all(COMPREHENSIVE_OSM.as_bytes())
            .expect("write fixture");
        let (_, path) = tmp.into_parts();

        let streamed = parse_osm_xml_file(&path).expect("streaming parse");
        let in_memory = parse_osm_xml_str(COMPREHENSIVE_OSM).expect("string parse");

        streamed.validate_invariants().expect("streamed invariants");
        in_memory
            .validate_invariants()
            .expect("in-memory invariants");

        // bounds (Option tuple of f64).
        assert_eq!(streamed.bounds, in_memory.bounds);

        // nodes (HashMap<i64, OsmNode>) — OsmNode has no PartialEq, so
        // compare lat/lon with a tight tolerance.
        assert_eq!(streamed.nodes.len(), in_memory.nodes.len());
        for (id, exp) in &in_memory.nodes {
            let got = streamed.nodes.get(id).expect("node present on both sides");
            assert!((got.lat - exp.lat).abs() < 1e-9, "node {id} lat");
            assert!((got.lon - exp.lon).abs() < 1e-9, "node {id} lon");
        }

        // ways (Vec<OsmWay>) — id, tags, node_refs.
        assert_eq!(streamed.ways.len(), in_memory.ways.len());
        for (i, (a, b)) in streamed.ways.iter().zip(in_memory.ways.iter()).enumerate() {
            assert_eq!(a.id, b.id, "way {i} id");
            assert_eq!(a.node_refs, b.node_refs, "way {i} node_refs");
            assert_eq!(a.tags, b.tags, "way {i} tags");
        }

        // relations (Vec<OsmRelation>) — tags + members (way_id, role).
        assert_eq!(streamed.relations.len(), in_memory.relations.len());
        for (i, (a, b)) in streamed
            .relations
            .iter()
            .zip(in_memory.relations.iter())
            .enumerate()
        {
            assert_eq!(a.tags, b.tags, "relation {i} tags");
            assert_eq!(
                a.members.len(),
                b.members.len(),
                "relation {i} member count"
            );
            for (j, (m, n)) in a.members.iter().zip(b.members.iter()).enumerate() {
                assert_eq!(m.way_id, n.way_id, "relation {i} member {j} way_id");
                assert_eq!(m.role, n.role, "relation {i} member {j} role");
            }
        }

        // poi_nodes / addr_nodes (Vec<OsmPoiNode>) — lat/lon, tags, source.
        assert_eq!(streamed.poi_nodes.len(), in_memory.poi_nodes.len());
        for (i, (a, b)) in streamed
            .poi_nodes
            .iter()
            .zip(in_memory.poi_nodes.iter())
            .enumerate()
        {
            assert!((a.lat - b.lat).abs() < 1e-9, "poi {i} lat");
            assert!((a.lon - b.lon).abs() < 1e-9, "poi {i} lon");
            assert_eq!(a.tags, b.tags, "poi {i} tags");
            assert_eq!(a.source, b.source, "poi {i} source");
        }
        assert_eq!(streamed.addr_nodes.len(), in_memory.addr_nodes.len());
        for (i, (a, b)) in streamed
            .addr_nodes
            .iter()
            .zip(in_memory.addr_nodes.iter())
            .enumerate()
        {
            assert!((a.lat - b.lat).abs() < 1e-9, "addr {i} lat");
            assert!((a.lon - b.lon).abs() < 1e-9, "addr {i} lon");
            assert_eq!(a.tags, b.tags, "addr {i} tags");
            assert_eq!(a.source, b.source, "addr {i} source");
        }

        // tree_nodes (Vec<OsmNode>) — lat/lon only.
        assert_eq!(streamed.tree_nodes.len(), in_memory.tree_nodes.len());
        for (i, (a, b)) in streamed
            .tree_nodes
            .iter()
            .zip(in_memory.tree_nodes.iter())
            .enumerate()
        {
            assert!((a.lat - b.lat).abs() < 1e-9, "tree {i} lat");
            assert!((a.lon - b.lon).abs() < 1e-9, "tree {i} lon");
        }
    }

    #[test]
    fn parse_osm_xml_file_returns_clear_error_on_missing_file() {
        // Pick a path under the temp dir that is guaranteed not to exist;
        // remove_file ignores NotFound so any leftover from a prior run is
        // cleared before the assertion.
        let path = std::env::temp_dir().join("par-osm-rust-arc013-definitely-missing.osm");
        let _ = std::fs::remove_file(&path);

        let err = match parse_osm_xml_file(&path) {
            Ok(_) => panic!("missing file must error, got Ok"),
            Err(e) => e,
        };

        // The top-level context must mention the file-open failure and the
        // path; the underlying OS NotFound is preserved in the error chain.
        let msg = err.to_string();
        assert!(
            msg.contains("opening"),
            "expected 'opening' in error message, got: {msg}"
        );
        let chain: Vec<String> = err.chain().map(|c| c.to_string()).collect();
        let full = chain.join(" -> ");
        assert!(
            full.contains(path.file_name().unwrap().to_string_lossy().as_ref()),
            "expected path in error chain, got: {full}"
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // QA-101: duplicate / missing way-id handling at the parse boundary.
    // The parser must skip-and-warn (not panic the `OsmData::new` debug
    // invariant, not silently collide on id 0). First occurrence wins for
    // duplicates, matching `OsmData::merge`.
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn parse_xml_skips_duplicate_way_id_keeping_first() {
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1" lat="0.0" lon="0.0"/>
  <node id="2" lat="0.0" lon="0.1"/>
  <way id="10">
    <nd ref="1"/>
    <nd ref="2"/>
    <tag k="highway" v="residential"/>
    <tag k="name" v="First"/>
  </way>
  <way id="10">
    <nd ref="1"/>
    <nd ref="2"/>
    <tag k="highway" v="primary"/>
    <tag k="name" v="Second"/>
  </way>
</osm>"#;
        let data = parse_osm_xml_str(xml).expect("parse must succeed");
        // Debug invariant (would have panicked pre-QA-101 with two ways at
        // id 10 colliding in ways_by_id).
        data.validate_invariants().expect("invariants hold");
        // Exactly one way survived; it is the FIRST occurrence.
        assert_eq!(data.ways.len(), 1);
        assert_eq!(data.ways[0].id, 10);
        assert_eq!(data.ways[0].tags["name"], "First");
        assert_eq!(data.ways[0].tags["highway"], "residential");
    }

    #[test]
    fn parse_xml_skips_way_with_missing_id_and_keeps_nodes() {
        // No `id` attribute on the way → skip the way entirely. Surrounding
        // nodes must still be registered.
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1" lat="0.0" lon="0.0"/>
  <node id="2" lat="0.0" lon="0.1"/>
  <way>
    <nd ref="1"/>
    <nd ref="2"/>
    <tag k="highway" v="residential"/>
  </way>
  <way id="30">
    <nd ref="1"/>
    <tag k="highway" v="track"/>
  </way>
</osm>"#;
        let data = parse_osm_xml_str(xml).expect("parse must succeed");
        data.validate_invariants().expect("invariants hold");
        // Both nodes retained (way skip does not prune nodes).
        assert_eq!(data.nodes.len(), 2);
        // Only the way with an id survived.
        assert_eq!(data.ways.len(), 1);
        assert_eq!(data.ways[0].id, 30);
    }

    #[test]
    fn parse_xml_skips_way_with_unparseable_id() {
        // Non-numeric `id` parses to None → same skip path as missing id.
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1" lat="0.0" lon="0.0"/>
  <way id="not-a-number">
    <nd ref="1"/>
  </way>
</osm>"#;
        let data = parse_osm_xml_str(xml).expect("parse must succeed");
        data.validate_invariants().expect("invariants hold");
        assert_eq!(data.nodes.len(), 1);
        assert!(data.ways.is_empty());
    }

    #[test]
    fn parse_xml_accepts_explicit_zero_way_id() {
        // The XML parser distinguishes "id attribute absent" (skip) from
        // "id attribute present with value 0" (accept — it is a legitimate,
        // if unusual, OSM id). PBF cannot make this distinction at the wire
        // level (its id field defaults to 0 when absent), so the PBF parser
        // treats id==0 as missing; this test pins the XML side explicitly.
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1" lat="0.0" lon="0.0"/>
  <way id="0">
    <nd ref="1"/>
  </way>
</osm>"#;
        let data = parse_osm_xml_str(xml).expect("parse must succeed");
        data.validate_invariants().expect("invariants hold");
        assert_eq!(data.ways.len(), 1);
        assert_eq!(data.ways[0].id, 0);
    }

    #[test]
    fn parse_osm_xml_file_skips_duplicate_way_id_on_streaming_path() {
        use std::io::Write;
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1" lat="0.0" lon="0.0"/>
  <node id="2" lat="0.0" lon="0.1"/>
  <way id="10">
    <nd ref="1"/>
    <nd ref="2"/>
    <tag k="name" v="First"/>
  </way>
  <way id="10">
    <nd ref="1"/>
    <tag k="name" v="Second"/>
  </way>
</osm>"#;
        let mut tmp = tempfile::Builder::new()
            .suffix(".osm")
            .tempfile()
            .expect("tempfile creation");
        tmp.write_all(xml.as_bytes()).expect("write fixture");
        let (_, path) = tmp.into_parts();

        let data = parse_osm_xml_file(&path).expect("streaming parse");
        data.validate_invariants().expect("invariants hold");
        assert_eq!(data.ways.len(), 1);
        assert_eq!(data.ways[0].id, 10);
        assert_eq!(data.ways[0].tags["name"], "First");
    }

    #[test]
    fn parse_osm_xml_file_skips_way_with_missing_id_on_streaming_path() {
        use std::io::Write;
        let xml = r#"<?xml version="1.0"?>
<osm version="0.6">
  <node id="1" lat="0.0" lon="0.0"/>
  <way>
    <nd ref="1"/>
    <tag k="highway" v="residential"/>
  </way>
</osm>"#;
        let mut tmp = tempfile::Builder::new()
            .suffix(".osm")
            .tempfile()
            .expect("tempfile creation");
        tmp.write_all(xml.as_bytes()).expect("write fixture");
        let (_, path) = tmp.into_parts();

        let data = parse_osm_xml_file(&path).expect("streaming parse");
        data.validate_invariants().expect("invariants hold");
        assert_eq!(data.nodes.len(), 1);
        assert!(data.ways.is_empty());
    }
}
