//! OSM XML serializer.
//!
//! Exposes [`write_osm_xml_string`], which turns an [`OsmData`] back into the
//! simple OSM XML dialect this crate and `osm-world` can re-parse.

use std::collections::{HashMap, HashSet};

use crate::synthetic_ids::{
    SYNTHETIC_NODE_ID_BASE, next_writer_node_id, writer_relation_id, writer_way_id,
};

use super::model::OsmData;

fn escape_xml_attr(value: &str) -> String {
    // QA-114: single-pass escape into one pre-sized `String`. The prior
    // chained `.replace()` allocated up to four intermediate Strings per
    // attribute (one per `replace` call); this version allocates once and
    // pushes each char's mapped entity (or the char itself) in place.
    //
    // The escape set mirrors the prior implementation exactly — `&`, `"`,
    // `<`, `>` — and **intentionally excludes `'`** so the output stays
    // byte-identical (the writer emits double-quoted attributes; apostrophes
    // are not special there). Round-trip tests pin this.
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

fn write_tags(xml: &mut String, tags: &HashMap<String, String>) {
    let mut entries: Vec<_> = tags.iter().collect();
    entries.sort_by_key(|(key, _)| *key);
    for (key, value) in entries {
        xml.push_str("    <tag k=\"");
        xml.push_str(&escape_xml_attr(key));
        xml.push_str("\" v=\"");
        xml.push_str(&escape_xml_attr(value));
        xml.push_str("\"/>\n");
    }
}

/// Serialize normalized [`OsmData`] into simple OSM XML that this crate and
/// `osm-world` can parse again.
///
/// The output is structurally valid OSM XML even when the source carries
/// partial data: ways that reference node IDs missing from `data.nodes` have
/// the dangling `<nd ref>` entries skipped (with a `log::warn!` per skipped
/// ref) so a downstream parser never receives a way pointing at a node that
/// does not exist.
///
/// # Examples
///
/// ```
/// use par_osm_rust::osm::{write_osm_xml_string, OsmData, OsmNode};
/// use std::collections::HashMap;
///
/// let data = OsmData::default()
///     .with_nodes(HashMap::from([(1, OsmNode { lat: 51.5, lon: -0.10 })]));
/// let xml = write_osm_xml_string(&data);
/// assert!(xml.contains("<node id=\"1\""));
/// ```
pub fn write_osm_xml_string(data: &OsmData) -> String {
    let mut xml =
        String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<osm version=\"0.6\">\n");

    if let Some((min_lat, min_lon, max_lat, max_lon)) = data.bounds {
        xml.push_str(&format!(
            "  <bounds minlat=\"{}\" minlon=\"{}\" maxlat=\"{}\" maxlon=\"{}\"/>\n",
            min_lat, min_lon, max_lat, max_lon
        ));
    }

    let mut nodes: Vec<_> = data.nodes.iter().collect();
    nodes.sort_by_key(|(id, _)| **id);
    for (id, node) in nodes {
        xml.push_str(&format!(
            "  <node id=\"{}\" lat=\"{}\" lon=\"{}\"/>\n",
            id, node.lat, node.lon
        ));
    }

    let mut occupied_node_ids: HashSet<i64> = data.nodes.keys().copied().collect();
    let mut synthetic_id = SYNTHETIC_NODE_ID_BASE;
    for poi in &data.poi_nodes {
        let node_id = next_writer_node_id(&mut synthetic_id, &mut occupied_node_ids);
        xml.push_str(&format!(
            "  <node id=\"{}\" lat=\"{}\" lon=\"{}\">\n",
            node_id, poi.lat, poi.lon
        ));
        write_tags(&mut xml, &poi.tags);
        xml.push_str("  </node>\n");
    }

    for addr in &data.addr_nodes {
        let node_id = next_writer_node_id(&mut synthetic_id, &mut occupied_node_ids);
        xml.push_str(&format!(
            "  <node id=\"{}\" lat=\"{}\" lon=\"{}\">\n",
            node_id, addr.lat, addr.lon
        ));
        write_tags(&mut xml, &addr.tags);
        xml.push_str("  </node>\n");
    }

    for tree in &data.tree_nodes {
        let node_id = next_writer_node_id(&mut synthetic_id, &mut occupied_node_ids);
        xml.push_str(&format!(
            "  <node id=\"{}\" lat=\"{}\" lon=\"{}\">\n    <tag k=\"natural\" v=\"tree\"/>\n  </node>\n",
            node_id, tree.lat, tree.lon
        ));
    }

    // QA-021 / ARC-003 / QA-001: each `OsmWay` carries its own id, so the
    // writer reads `way.id` directly — no per-way scan of `ways_by_id` and no
    // up-front inverse map. The `writer_way_id(idx)` synthetic fallback
    // remains as a safety net for an externally-constructed `OsmData` whose
    // `ways[].id` was left at the default 0; `validate_invariants` is the
    // real guard against drift between `ways[].id` and `ways_by_id`.
    for (idx, way) in data.ways.iter().enumerate() {
        let way_id = if way.id != 0 {
            way.id
        } else {
            writer_way_id(idx)
        };
        xml.push_str(&format!("  <way id=\"{}\">\n", way_id));
        for node_ref in &way.node_refs {
            // ARC-016: emit `<nd ref>` only for nodes that actually exist in
            // the dataset so the serialized XML is always structurally valid.
            // Skipping is logged so the data loss is observable; we never
            // panic on a partially-populated `OsmData`.
            if !data.nodes.contains_key(node_ref) {
                log::warn!(
                    "write_osm_xml_string: way {way_id} references missing node {node_ref}; skipping dangling <nd ref>"
                );
                continue;
            }
            xml.push_str(&format!("    <nd ref=\"{}\"/>\n", node_ref));
        }
        write_tags(&mut xml, &way.tags);
        xml.push_str("  </way>\n");
    }

    for (idx, relation) in data.relations.iter().enumerate() {
        let relation_id = writer_relation_id(idx);
        xml.push_str(&format!("  <relation id=\"{}\">\n", relation_id));
        for member in &relation.members {
            xml.push_str(&format!(
                "    <member type=\"way\" ref=\"{}\" role=\"{}\"/>\n",
                member.way_id,
                escape_xml_attr(&member.role)
            ));
        }
        write_tags(&mut xml, &relation.tags);
        xml.push_str("  </relation>\n");
    }

    xml.push_str("</osm>\n");
    xml
}
