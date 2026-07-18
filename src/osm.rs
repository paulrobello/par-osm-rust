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

use anyhow::{Context, Result};
use osmpbf::{Element, ElementReader};
use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::BytesStart;
use quick_xml::events::Event;
use quick_xml::events::attributes::Attribute;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use crate::synthetic_ids::{
    SYNTHETIC_NODE_ID_BASE, next_writer_node_id, writer_relation_id, writer_way_id,
};

/// A geographic point from the OSM dataset.
#[derive(Debug, Clone, Copy)]
pub struct OsmNode {
    pub lat: f64,
    pub lon: f64,
}

/// Data source for normalized map features.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureSource {
    #[default]
    Osm,
    Overture,
    Synthetic,
}

/// An OSM node that carries feature tags (amenity, shop, tourism, etc.).
/// Used for POI marker placement.
#[derive(Debug, Clone)]
pub struct OsmPoiNode {
    pub lat: f64,
    pub lon: f64,
    pub tags: HashMap<String, String>,
    pub source: FeatureSource,
}

/// An OSM way: an ordered sequence of node references with tags.
///
/// `id` is the way's own OSM identifier; it is the single source of truth
/// consumed by [`OsmData::new`], the XML writer, and `ways_by_id`. Keeping
/// the id on the struct (QA-021) obsoletes the prior `(id, way)` pair
/// plumbing and the writer's reverse way-id lookup (ARC-003 / QA-001).
#[derive(Debug, Clone)]
pub struct OsmWay {
    pub id: i64,
    pub tags: HashMap<String, String>,
    pub node_refs: Vec<i64>,
}

/// A member of an OSM relation with its role.
#[derive(Debug, Clone)]
pub struct RelationMember {
    /// Way ID referenced by this member.
    pub way_id: i64,
    /// Role string (e.g. "outer", "inner").
    pub role: String,
}

/// An OSM relation: a collection of ways with roles and tags.
#[derive(Debug, Clone)]
pub struct OsmRelation {
    pub tags: HashMap<String, String>,
    pub members: Vec<RelationMember>,
}

/// Parsed OSM dataset.
///
/// Every collection is `pub(crate)` so external consumers must go through the
/// accessors below. The `ways` / `ways_by_id` pair in particular must stay
/// in lock-step: each entry in `ways` has exactly one corresponding entry in
/// `ways_by_id` mapping its OSM id to its index. The pair is mutated only by
/// [`OsmData::new`] and [`OsmData::push_way`]; in-place bulk operations
/// (`merge`, `clip_to_bbox`) preserve the invariant internally and are
/// checked by [`OsmData::validate_invariants`] in debug builds.
pub struct OsmData {
    /// All nodes keyed by OSM id.
    pub(crate) nodes: HashMap<i64, OsmNode>,
    /// Ways in insertion order.
    pub(crate) ways: Vec<OsmWay>,
    /// Way lookup by ID for relation member resolution.
    ///
    /// Maps each OSM way ID to its position in the `ways` vector. Storing an
    /// index avoids duplicating `OsmWay` values while still allowing relation
    /// members to find their referenced ways efficiently. Maintained
    /// exclusively by [`OsmData::new`] and [`OsmData::push_way`].
    pub(crate) ways_by_id: HashMap<i64, usize>,
    /// Multipolygon relations.
    pub relations: Vec<OsmRelation>,
    /// Bounding box: (min_lat, min_lon, max_lat, max_lon)
    pub bounds: Option<(f64, f64, f64, f64)>,
    /// Standalone nodes with POI tags (amenity, shop, tourism, leisure, historic).
    pub poi_nodes: Vec<OsmPoiNode>,
    /// Standalone nodes with address tags (addr:housenumber).
    /// These are typically entrance/door nodes placed on building outlines in OSM.
    pub addr_nodes: Vec<OsmPoiNode>,
    /// Individual tree positions (from OSM `natural=tree` or Overture `land/tree`).
    pub tree_nodes: Vec<OsmNode>,
}

impl OsmData {
    /// Construct an [`OsmData`] from already-assembled collections, seeding
    /// `ways_by_id` from each [`OsmWay::id`].
    ///
    /// This is the single place the `ways` / `ways_by_id` invariant is
    /// established: the constructor iterates `ways` in order and records
    /// `ways_by_id[way.id] = index`. Callers must populate [`OsmWay::id`]
    /// before passing ways in (QA-021).
    ///
    /// # Examples
    ///
    /// ```
    /// use par_osm_rust::osm::{OsmData, OsmNode};
    /// use std::collections::HashMap;
    ///
    /// let data = OsmData::new(
    ///     HashMap::from([(1, OsmNode { lat: 51.5, lon: -0.12 })]),
    ///     Vec::new(),
    ///     Vec::new(),
    ///     None,
    ///     Vec::new(),
    ///     Vec::new(),
    ///     Vec::new(),
    /// );
    /// assert_eq!(data.iter_ways().count(), 0);
    /// ```
    pub fn new(
        nodes: HashMap<i64, OsmNode>,
        ways: Vec<OsmWay>,
        relations: Vec<OsmRelation>,
        bounds: Option<(f64, f64, f64, f64)>,
        poi_nodes: Vec<OsmPoiNode>,
        addr_nodes: Vec<OsmPoiNode>,
        tree_nodes: Vec<OsmNode>,
    ) -> Self {
        let ways_by_id = ways
            .iter()
            .enumerate()
            .map(|(idx, way)| (way.id, idx))
            .collect();
        let data = Self {
            nodes,
            ways,
            ways_by_id,
            relations,
            bounds,
            poi_nodes,
            addr_nodes,
            tree_nodes,
        };
        debug_assert!(
            data.validate_invariants().is_ok(),
            "OsmData::new produced an inconsistent state"
        );
        data
    }

    /// Append a way, updating `ways_by_id` atomically from [`OsmWay::id`].
    ///
    /// This is the single sanctioned mutation path for incrementally adding
    /// ways to an existing [`OsmData`]. Callers that already have a full
    /// sequence should prefer [`OsmData::new`].
    pub fn push_way(&mut self, way: OsmWay) {
        let idx = self.ways.len();
        let id = way.id;
        self.ways.push(way);
        self.ways_by_id.insert(id, idx);
        debug_assert!(
            self.validate_invariants().is_ok(),
            "OsmData::push_way produced an inconsistent state"
        );
    }

    /// Borrow the ways slice in insertion order.
    pub fn iter_ways(&self) -> impl Iterator<Item = &OsmWay> {
        self.ways.iter()
    }

    /// Return the OSM id of the way at `index`, or `None` if the index is
    /// out of range. Reads [`OsmWay::id`] directly (QA-021).
    pub fn way_id_at(&self, index: usize) -> Option<i64> {
        self.ways.get(index).map(|way| way.id)
    }

    /// Verify the `ways` / `ways_by_id` invariant: equal lengths, every
    /// stored index is in range, no two ids share an index, and each
    /// `ways_by_id[ways[idx].id] == idx` (the per-way consistency check
    /// added in QA-021, since `ways[].id` is now the source of truth).
    ///
    /// Returns `Err(message)` on the first violation. Called automatically
    /// in debug builds from [`OsmData::new`] and [`OsmData::push_way`];
    /// downstream consumers may call it directly when they want to defend
    /// against an externally-constructed [`OsmData`].
    pub fn validate_invariants(&self) -> Result<(), String> {
        if self.ways_by_id.len() != self.ways.len() {
            return Err(format!(
                "ways_by_id length {} != ways length {}",
                self.ways_by_id.len(),
                self.ways.len()
            ));
        }
        let mut seen_indices: HashSet<usize> = HashSet::with_capacity(self.ways_by_id.len());
        for &idx in self.ways_by_id.values() {
            if idx >= self.ways.len() {
                return Err(format!(
                    "ways_by_id references index {idx} >= ways length {}",
                    self.ways.len()
                ));
            }
            if !seen_indices.insert(idx) {
                return Err(format!("duplicate ways_by_id index {idx}"));
            }
        }
        for (idx, way) in self.ways.iter().enumerate() {
            match self.ways_by_id.get(&way.id) {
                Some(&stored_idx) if stored_idx == idx => {}
                Some(&stored_idx) => {
                    return Err(format!(
                        "ways[{idx}].id {} maps to ways_by_id index {stored_idx}, expected {idx}",
                        way.id
                    ));
                }
                None => {
                    return Err(format!(
                        "ways[{idx}].id {} is missing from ways_by_id",
                        way.id
                    ));
                }
            }
        }
        Ok(())
    }

    /// Merge `other` into `self`, combining every collection [`OsmData`] holds.
    ///
    /// This is the central mutation on the central data type, so the contract
    /// is enumerated in full:
    ///
    /// * `nodes` — extended from `other` via `HashMap::extend`. **Collision
    ///   semantics: last-write-wins** — a node ID present in both `self` and
    ///   `other` keeps `other`'s value. (Safe by construction today because
    ///   distinct fetches mint disjoint IDs, but the documented contract is
    ///   last-write-wins should a future caller allow collisions; see QA-015.)
    /// * `ways` — `other`'s ways are appended in order; their indices in
    ///   `ways_by_id` are shifted by `self.ways.len()` so each `(id → index)`
    ///   entry still points at the right slot. Same last-write-wins collision
    ///   rule applies to `ways_by_id` if a way ID appears on both sides.
    /// * `relations` — `other`'s relations are appended; no de-duplication.
    /// * `poi_nodes`, `addr_nodes`, `tree_nodes` — `other`'s entries are
    ///   appended in order; no de-duplication.
    /// * `bounds` — when both sides have a bbox, the per-axis union is stored
    ///   `(min(min_lat), min(min_lon), max(max_lat), max(max_lon))`. When only
    ///   one side has a bbox, that bbox is kept. When neither side has one,
    ///   `bounds` remain `None`.
    pub fn merge(&mut self, other: OsmData) {
        self.nodes.extend(other.nodes);
        let offset = self.ways.len();
        self.ways.extend(other.ways);
        // Adjust indices from `other` to account for the ways already in `self`.
        self.ways_by_id.extend(
            other
                .ways_by_id
                .into_iter()
                .map(|(id, idx)| (id, idx + offset)),
        );
        self.relations.extend(other.relations);
        self.poi_nodes.extend(other.poi_nodes);
        self.addr_nodes.extend(other.addr_nodes);
        self.tree_nodes.extend(other.tree_nodes);
        match (self.bounds, other.bounds) {
            (Some((a0, a1, a2, a3)), Some((b0, b1, b2, b3))) => {
                self.bounds = Some((a0.min(b0), a1.min(b1), a2.max(b2), a3.max(b3)));
            }
            (None, b) => self.bounds = b,
            _ => {}
        }
    }

    /// Clip data to a bounding box, keeping only features that touch the bbox.
    ///
    /// `bbox` is `(min_lat, min_lon, max_lat, max_lon)`.
    /// Ways are kept if at least one node falls inside the bbox.
    /// POI and address nodes are kept only if inside the bbox.
    /// Unreferenced nodes are pruned.
    pub fn clip_to_bbox(&mut self, bbox: (f64, f64, f64, f64)) {
        let (min_lat, min_lon, max_lat, max_lon) = bbox;

        let in_bbox = |lat: f64, lon: f64| -> bool {
            lat >= min_lat && lat <= max_lat && lon >= min_lon && lon <= max_lon
        };

        // Filter ways: keep if any node is inside the bbox. `ways[].id` is
        // the source of truth (QA-021), so iterate `ways` directly and clone
        // the survivors; `ways_by_id` is rebuilt from each kept way's `id`.
        let mut keep_node_ids: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let mut kept_ways: Vec<OsmWay> = Vec::new();
        for way in &self.ways {
            let touches_bbox = way
                .node_refs
                .iter()
                .any(|id| self.nodes.get(id).is_some_and(|n| in_bbox(n.lat, n.lon)));
            if touches_bbox {
                for id in &way.node_refs {
                    keep_node_ids.insert(*id);
                }
                kept_ways.push(way.clone());
            }
        }

        // Rebuild the ways / ways_by_id pair atomically from the kept ways.
        self.ways_by_id = kept_ways
            .iter()
            .enumerate()
            .map(|(idx, way)| (way.id, idx))
            .collect();
        self.ways = kept_ways;

        // Prune nodes to only those referenced by kept ways
        self.nodes.retain(|id, _| keep_node_ids.contains(id));

        // Filter POI and address nodes
        self.poi_nodes.retain(|p| in_bbox(p.lat, p.lon));
        self.addr_nodes.retain(|p| in_bbox(p.lat, p.lon));
        self.tree_nodes.retain(|n| in_bbox(n.lat, n.lon));

        // Filter relations: keep if any member way was kept
        self.relations.retain(|rel| {
            rel.members
                .iter()
                .any(|m| self.ways_by_id.contains_key(&m.way_id))
        });

        // Update bounds to the requested bbox
        self.bounds = Some(bbox);

        debug_assert!(
            self.validate_invariants().is_ok(),
            "OsmData::clip_to_bbox produced an inconsistent state"
        );
    }
}

fn xml_attr_value(attr: &Attribute<'_>) -> String {
    attr.normalized_value(XmlVersion::Implicit1_0)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| {
            std::str::from_utf8(attr.value.as_ref())
                .unwrap_or("")
                .to_string()
        })
}

fn xml_attr_parse<T: std::str::FromStr>(attr: &Attribute<'_>) -> Option<T> {
    xml_attr_value(attr).parse().ok()
}

/// Shared body of the `Element::Node` and `Element::DenseNode` branches in
/// [`parse_pbf`]. The two osmpbf element types expose the same `(id, lat, lon,
/// tags)` surface but through distinct types with no public shared trait, so
/// the branches resolve those four values and hand them off here (QA-004).
///
/// Updates the running bbox accumulator (`min_lat`/`min_lon`/`max_lat`/
/// `max_lon`), inserts the node into `nodes`, and classifies the tags to push
/// the node into the appropriate feature collection(s).
///
/// Each parameter maps 1:1 to a local accumulator already in scope at the
/// call site; the long signature is the cost of extracting the duplication
/// without restructuring the surrounding `parse_pbf` body.
#[allow(clippy::too_many_arguments)]
fn process_pbf_node(
    id: i64,
    lat: f64,
    lon: f64,
    tags: HashMap<String, String>,
    nodes: &mut HashMap<i64, OsmNode>,
    poi_nodes: &mut Vec<OsmPoiNode>,
    addr_nodes: &mut Vec<OsmPoiNode>,
    tree_nodes: &mut Vec<OsmNode>,
    min_lat: &mut f64,
    min_lon: &mut f64,
    max_lat: &mut f64,
    max_lon: &mut f64,
) {
    *min_lat = min_lat.min(lat);
    *min_lon = min_lon.min(lon);
    *max_lat = max_lat.max(lat);
    *max_lon = max_lon.max(lon);
    nodes.insert(id, OsmNode { lat, lon });
    if tags.keys().any(|k| {
        matches!(
            k.as_str(),
            "amenity" | "shop" | "tourism" | "leisure" | "historic"
        )
    }) {
        poi_nodes.push(OsmPoiNode {
            lat,
            lon,
            tags: tags.clone(),
            source: FeatureSource::Osm,
        });
    }
    if tags.contains_key("addr:housenumber") {
        addr_nodes.push(OsmPoiNode {
            lat,
            lon,
            tags: tags.clone(),
            source: FeatureSource::Osm,
        });
    }
    if tags.get("natural").map(|s| s.as_str()) == Some("tree") {
        tree_nodes.push(OsmNode { lat, lon });
    }
}

/// Parse a `.osm.pbf` file into a full [`OsmData`].
///
/// Returns nodes, ways (each paired with its OSM id), multipolygon relations,
/// POI nodes entries (nodes carrying `amenity`/`shop`/`tourism`/`leisure`/
/// `historic`), address node entries (nodes carrying `addr:housenumber`),
/// individual tree positions (nodes carrying `natural=tree`), and the
/// dataset bounding box computed from the observed node lat/lon extrema.
///
/// # File-provenance precondition (SEC-008)
///
/// `parse_pbf` memory-maps the file at `path` via `osmpbf`'s
/// `ElementReader::from_path`. The caller MUST ensure the file is not
/// truncated, replaced, or concurrently modified for the duration of the
/// call. A mapping whose backing file shrinks underneath the reader can
/// raise `SIGBUS` when the OS revokes a page that no longer exists. For
/// untrusted or concurrently-written inputs, copy the file to a stable path
/// first and parse the copy.
///
/// # Errors
///
/// Returns an error if the file cannot be opened or read, or if the PBF
/// blob stream is malformed.
///
/// # Examples
///
/// ```no_run
/// use par_osm_rust::osm::parse_pbf;
/// use std::path::Path;
///
/// # fn main() -> anyhow::Result<()> {
/// let data = parse_pbf(Path::new("/path/to/planet.osm.pbf"))?;
/// println!("{} ways", data.iter_ways().count());
/// # Ok(())
/// # }
/// ```
pub fn parse_pbf(path: &Path) -> Result<OsmData> {
    let reader =
        ElementReader::from_path(path).with_context(|| format!("opening {}", path.display()))?;

    let mut nodes: HashMap<i64, OsmNode> = HashMap::new();
    let mut ways: Vec<OsmWay> = Vec::new();
    let mut relations: Vec<OsmRelation> = Vec::new();
    let mut poi_nodes: Vec<OsmPoiNode> = Vec::new();
    let mut addr_nodes: Vec<OsmPoiNode> = Vec::new();
    let mut tree_nodes: Vec<OsmNode> = Vec::new();
    let mut min_lat = f64::MAX;
    let mut min_lon = f64::MAX;
    let mut max_lat = f64::MIN;
    let mut max_lon = f64::MIN;

    reader
        .for_each(|element| match element {
            Element::Node(n) => {
                let tags: HashMap<String, String> = n
                    .tags()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                process_pbf_node(
                    n.id(),
                    n.lat(),
                    n.lon(),
                    tags,
                    &mut nodes,
                    &mut poi_nodes,
                    &mut addr_nodes,
                    &mut tree_nodes,
                    &mut min_lat,
                    &mut min_lon,
                    &mut max_lat,
                    &mut max_lon,
                );
            }
            Element::DenseNode(n) => {
                let tags: HashMap<String, String> = n
                    .tags()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                process_pbf_node(
                    n.id(),
                    n.lat(),
                    n.lon(),
                    tags,
                    &mut nodes,
                    &mut poi_nodes,
                    &mut addr_nodes,
                    &mut tree_nodes,
                    &mut min_lat,
                    &mut min_lon,
                    &mut max_lat,
                    &mut max_lon,
                );
            }
            Element::Way(w) => {
                let tags: HashMap<String, String> = w
                    .tags()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                let node_refs: Vec<i64> = w.refs().collect();
                let way = OsmWay {
                    id: w.id(),
                    tags: tags.clone(),
                    node_refs: node_refs.clone(),
                };
                ways.push(way);
            }
            Element::Relation(r) => {
                let rel_type = r.tags().find(|(k, _)| *k == "type").map(|(_, v)| v);
                if rel_type == Some("multipolygon") {
                    let tags: HashMap<String, String> = r
                        .tags()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect();
                    let members: Vec<RelationMember> = r
                        .members()
                        .filter_map(|m| {
                            if matches!(m.member_type, osmpbf::elements::RelMemberType::Way) {
                                Some(RelationMember {
                                    way_id: m.member_id,
                                    role: m.role().unwrap_or_default().to_string(),
                                })
                            } else {
                                None
                            }
                        })
                        .collect();
                    if !members.is_empty() {
                        relations.push(OsmRelation { tags, members });
                    }
                }
            }
        })
        .context("reading PBF elements")?;

    let bounds = if min_lat < f64::MAX {
        Some((min_lat, min_lon, max_lat, max_lon))
    } else {
        None
    };

    log::info!(
        "Parsed {} nodes, {} ways, {} relations, {} POI nodes, {} address nodes, {} tree nodes",
        nodes.len(),
        ways.len(),
        relations.len(),
        poi_nodes.len(),
        addr_nodes.len(),
        tree_nodes.len()
    );

    Ok(OsmData::new(
        nodes, ways, relations, bounds, poi_nodes, addr_nodes, tree_nodes,
    ))
}

/// Maximum element nesting depth accepted by [`parse_osm_xml_str`].
///
/// quick-xml 0.41 is XXE/billion-laughs-safe by default, but unbounded
/// element nesting is the one residual denial-of-service vector. OSM XML
/// is effectively flat (depth 2-3: `<osm>` -> `<node>`/`<way>`/`<relation>`
/// -> `<tag>`/`<nd>`/`<member>`), so 64 is far above any legitimate input
/// while still bounding stack growth from a malicious payload. SEC-004.
const MAX_XML_DEPTH: usize = 64;

/// Read the `minlat`/`minlon`/`maxlat`/`maxlon` attributes from a
/// `<bounds>` element. Returns `None` if any of the four required
/// attributes is missing or unparseable.
fn parse_bounds_attrs(e: &BytesStart<'_>) -> Option<(f64, f64, f64, f64)> {
    let mut minlat: Option<f64> = None;
    let mut minlon: Option<f64> = None;
    let mut maxlat: Option<f64> = None;
    let mut maxlon: Option<f64> = None;
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"minlat" => minlat = xml_attr_parse(&attr),
            b"minlon" => minlon = xml_attr_parse(&attr),
            b"maxlat" => maxlat = xml_attr_parse(&attr),
            b"maxlon" => maxlon = xml_attr_parse(&attr),
            _ => {}
        }
    }
    Some((minlat?, minlon?, maxlat?, maxlon?))
}

/// Read the `id`/`lat`/`lon` attributes from a `<node>` element. Returns
/// `None` if any of the three required attributes is missing or unparseable.
fn parse_node_attrs(e: &BytesStart<'_>) -> Option<(i64, f64, f64)> {
    let mut id: Option<i64> = None;
    let mut lat: Option<f64> = None;
    let mut lon: Option<f64> = None;
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"id" => id = xml_attr_parse(&attr),
            b"lat" => lat = xml_attr_parse(&attr),
            b"lon" => lon = xml_attr_parse(&attr),
            _ => {}
        }
    }
    Some((id?, lat?, lon?))
}

/// Read the `k`/`v` attributes from a `<tag>` element. Returns `None` when
/// `k` is missing or empty, matching the parser's long-standing skip
/// behavior for tags without a key.
fn parse_tag_attrs(e: &BytesStart<'_>) -> Option<(String, String)> {
    let mut k = String::new();
    let mut v = String::new();
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"k" => k = xml_attr_value(&attr),
            b"v" => v = xml_attr_value(&attr),
            _ => {}
        }
    }
    if k.is_empty() { None } else { Some((k, v)) }
}

/// Read the `ref` attribute from an `<nd>` element. Returns `None` if
/// `ref` is missing or unparseable as an `i64`.
fn parse_nd_ref(e: &BytesStart<'_>) -> Option<i64> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == b"ref")
        .and_then(|a| xml_attr_parse::<i64>(&a))
}

/// Read the `type`/`ref`/`role` attributes from a `<member>` element.
/// Returns `(type, ref, role)` with `ref = 0` when missing/unparseable and
/// empty strings for `type`/`role` when absent. Callers decide which
/// member types to keep; the parser only retains `type="way"` members
/// with a non-zero ref.
fn parse_member_attrs(e: &BytesStart<'_>) -> (String, i64, String) {
    let mut mtype = String::new();
    let mut mref: i64 = 0;
    let mut mrole = String::new();
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"type" => mtype = xml_attr_value(&attr),
            b"ref" => mref = xml_attr_parse(&attr).unwrap_or(0),
            b"role" => mrole = xml_attr_value(&attr),
            _ => {}
        }
    }
    (mtype, mref, mrole)
}

/// Parse an OSM XML string into `OsmData`.
///
/// Single-pass: one `read_event` loop collects nodes, ways, relations, and
/// `<bounds>` in the order they appear, regardless of element ordering.
/// Overpass does not guarantee node-before-way ordering, but no position
/// resolution happens at parse time — ways store raw node-id references
/// (`OsmWay::node_refs`) and relations store raw way-id references
/// (`RelationMember::way_id`), so the parser does not need a complete
/// node-position map to emit ways or relations. Position resolution (e.g.
/// for clipping or rendering) is deferred to consumers.
///
/// Element nesting depth is capped at [`MAX_XML_DEPTH`] (SEC-004).
///
/// # Examples
///
/// ```
/// use par_osm_rust::osm::parse_osm_xml_str;
///
/// let xml = r#"<?xml version="1.0"?>
/// <osm version="0.6">
///   <node id="1" lat="51.5" lon="-0.10"/>
///   <node id="2" lat="51.5" lon="-0.09"/>
/// </osm>"#;
///
/// let data = parse_osm_xml_str(xml)?;
/// assert_eq!(data.iter_ways().count(), 0);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn parse_osm_xml_str(xml: &str) -> Result<OsmData> {
    let mut nodes: HashMap<i64, OsmNode> = HashMap::new();
    let mut ways: Vec<OsmWay> = Vec::new();
    let mut relations: Vec<OsmRelation> = Vec::new();
    let mut poi_nodes: Vec<OsmPoiNode> = Vec::new();
    let mut addr_nodes: Vec<OsmPoiNode> = Vec::new();
    let mut tree_nodes: Vec<OsmNode> = Vec::new();
    let mut min_lat = f64::MAX;
    let mut min_lon = f64::MAX;
    let mut max_lat = f64::MIN;
    let mut max_lon = f64::MIN;
    let mut explicit_bounds: Option<(f64, f64, f64, f64)> = None;

    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    // Per-element state. The three `in_*` flags are mutually exclusive
    // because OSM XML never nests `<node>`/`<way>`/`<relation>` inside
    // each other; their `<tag>`/`<nd>`/`<member>` children appear between
    // the Start and End events of the owning element.
    let mut in_node = false;
    let mut cur_lat = 0.0f64;
    let mut cur_lon = 0.0f64;
    let mut cur_node_tags: HashMap<String, String> = HashMap::new();

    let mut in_way = false;
    let mut current_way_id: i64 = 0;
    let mut current_tags: HashMap<String, String> = HashMap::new();
    let mut current_node_refs: Vec<i64> = Vec::new();

    let mut in_relation = false;
    let mut current_members: Vec<RelationMember> = Vec::new();

    let mut depth: usize = 0;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => {
                depth += 1;
                if depth > MAX_XML_DEPTH {
                    return Err(anyhow::anyhow!(
                        "XML element nesting depth {depth} exceeds limit {MAX_XML_DEPTH} at position {}",
                        reader.buffer_position()
                    ));
                }
                match e.name().as_ref() {
                    b"bounds" => {
                        if let Some(b) = parse_bounds_attrs(e) {
                            explicit_bounds = Some(b);
                        }
                    }
                    b"node" => {
                        if let Some((id, lat, lon)) = parse_node_attrs(e) {
                            min_lat = min_lat.min(lat);
                            min_lon = min_lon.min(lon);
                            max_lat = max_lat.max(lat);
                            max_lon = max_lon.max(lon);
                            nodes.insert(id, OsmNode { lat, lon });
                            in_node = true;
                            cur_lat = lat;
                            cur_lon = lon;
                            cur_node_tags.clear();
                        }
                    }
                    b"way" => {
                        in_way = true;
                        current_way_id = e
                            .attributes()
                            .flatten()
                            .find(|a| a.key.as_ref() == b"id")
                            .and_then(|a| xml_attr_parse::<i64>(&a))
                            .unwrap_or(0);
                        current_tags.clear();
                        current_node_refs.clear();
                    }
                    b"relation" => {
                        in_relation = true;
                        current_tags.clear();
                        current_members.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => match e.name().as_ref() {
                b"bounds" => {
                    if let Some(b) = parse_bounds_attrs(e) {
                        explicit_bounds = Some(b);
                    }
                }
                b"node" => {
                    if let Some((id, lat, lon)) = parse_node_attrs(e) {
                        min_lat = min_lat.min(lat);
                        min_lon = min_lon.min(lon);
                        max_lat = max_lat.max(lat);
                        max_lon = max_lon.max(lon);
                        nodes.insert(id, OsmNode { lat, lon });
                    }
                }
                b"tag" if in_node || in_way || in_relation => {
                    if let Some((k, v)) = parse_tag_attrs(e) {
                        if in_node {
                            cur_node_tags.insert(k, v);
                        } else {
                            current_tags.insert(k, v);
                        }
                    }
                }
                b"nd" if in_way => {
                    if let Some(r) = parse_nd_ref(e) {
                        current_node_refs.push(r);
                    }
                }
                b"member" if in_relation => {
                    let (mtype, mref, mrole) = parse_member_attrs(e);
                    if mtype == "way" && mref != 0 {
                        current_members.push(RelationMember {
                            way_id: mref,
                            role: mrole,
                        });
                    }
                }
                _ => {}
            },
            Ok(Event::End(ref e)) => {
                depth = depth.saturating_sub(1);
                match e.name().as_ref() {
                    b"node" if in_node => {
                        in_node = false;
                        if cur_node_tags.keys().any(|k| {
                            matches!(
                                k.as_str(),
                                "amenity" | "shop" | "tourism" | "leisure" | "historic"
                            )
                        }) {
                            poi_nodes.push(OsmPoiNode {
                                lat: cur_lat,
                                lon: cur_lon,
                                tags: cur_node_tags.clone(),
                                source: FeatureSource::Osm,
                            });
                        }
                        if cur_node_tags.contains_key("addr:housenumber") {
                            addr_nodes.push(OsmPoiNode {
                                lat: cur_lat,
                                lon: cur_lon,
                                tags: cur_node_tags.clone(),
                                source: FeatureSource::Osm,
                            });
                        }
                        if cur_node_tags.get("natural").map(|s| s.as_str()) == Some("tree") {
                            tree_nodes.push(OsmNode {
                                lat: cur_lat,
                                lon: cur_lon,
                            });
                        }
                    }
                    b"way" if in_way => {
                        in_way = false;
                        let way = OsmWay {
                            id: current_way_id,
                            tags: current_tags.clone(),
                            node_refs: current_node_refs.clone(),
                        };
                        ways.push(way);
                    }
                    b"relation" if in_relation => {
                        in_relation = false;
                        let rel_type = current_tags.get("type").map(|s| s.as_str());
                        if rel_type == Some("multipolygon") && !current_members.is_empty() {
                            relations.push(OsmRelation {
                                tags: current_tags.clone(),
                                members: current_members.clone(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "XML parse error at position {}: {e}",
                    reader.buffer_position()
                ));
            }
            _ => {}
        }
    }

    let bounds = explicit_bounds
        .or_else(|| (min_lat < f64::MAX).then_some((min_lat, min_lon, max_lat, max_lon)));

    log::info!(
        "Parsed {} nodes, {} ways, {} relations, {} POI nodes, {} address nodes, {} tree nodes (XML)",
        nodes.len(),
        ways.len(),
        relations.len(),
        poi_nodes.len(),
        addr_nodes.len(),
        tree_nodes.len(),
    );

    Ok(OsmData::new(
        nodes, ways, relations, bounds, poi_nodes, addr_nodes, tree_nodes,
    ))
}

/// Stream a `.osm` XML file from disk into [`OsmData`] without first
/// reading the whole file into memory (ARC-013).
///
/// **Stream design.** The file is opened, wrapped in a `std::io::BufReader<File>`,
/// and handed to `quick_xml::Reader::from_reader`, which pulls bytes
/// incrementally. The event loop uses `quick_xml::Reader::read_event_into`
/// with a reused scratch `Vec<u8>` — the buffer-reusing API that a
/// streaming reader requires (unlike [`parse_osm_xml_str`], which borrows
/// directly from its input `&str` via `read_event`).
///
/// **Parser semantics are identical to [`parse_osm_xml_str`].** The same
/// single-pass structure runs against the streamed events: nodes, ways,
/// relations, and `<bounds>` are collected in arrival order regardless of
/// element ordering (Overpass does not guarantee node-before-way); the
/// same attribute helpers (`parse_bounds_attrs`, `parse_node_attrs`,
/// `parse_tag_attrs`, `parse_nd_ref`, `parse_member_attrs`) decode each
/// element; the [`MAX_XML_DEPTH`] cap (SEC-004) bounds element nesting;
/// and the resulting [`OsmWay`]s carry their OSM id (QA-021), fed into
/// [`OsmData::new`]. For any valid file the output equals
/// `parse_osm_xml_str(&std::fs::read_to_string(path)?)`.
///
/// **Memory profile.** Peak memory is bounded by:
/// * the `BufReader` internal buffer (8 KiB by default),
/// * the per-event scratch `Vec<u8>` (grows to the largest single XML
///   event — typically a few hundred bytes for OSM elements),
/// * the accumulated [`OsmData`] itself (inherent to the dataset).
///
/// The peak is **not** proportional to the input file size:
/// `std::fs::read_to_string`'s full-file `String` (roughly file-size bytes
/// on top of the parsed [`OsmData`]) is avoided. For a 200 MB `.osm`
/// extract that is the difference between ~400 MB peak (in-memory string
/// plus parsed data) and roughly the parsed data alone.
///
/// # Errors
///
/// Returns an error if the file cannot be opened or read, or if the XML
/// is malformed (including the [`MAX_XML_DEPTH`] violation, SEC-004).
///
/// # Examples
///
/// ```no_run
/// use par_osm_rust::osm::parse_osm_xml_file;
///
/// # fn main() -> anyhow::Result<()> {
/// let data = parse_osm_xml_file("/path/to/extract.osm")?;
/// println!("{} ways", data.iter_ways().count());
/// # Ok(())
/// # }
/// ```
pub fn parse_osm_xml_file<P: AsRef<Path>>(path: P) -> Result<OsmData> {
    let path_ref = path.as_ref();
    let file = File::open(path_ref).with_context(|| format!("opening {}", path_ref.display()))?;
    let bufreader = BufReader::new(file);
    let mut reader = Reader::from_reader(bufreader);
    reader.config_mut().trim_text(true);

    // Per-element state — mirrors parse_osm_xml_str exactly. The three
    // `in_*` flags are mutually exclusive because OSM XML never nests
    // `<node>`/`<way>`/`<relation>` inside each other; their `<tag>`/`<nd>`/
    // `<member>` children appear between the Start and End events of the
    // owning element.
    let mut nodes: HashMap<i64, OsmNode> = HashMap::new();
    let mut ways: Vec<OsmWay> = Vec::new();
    let mut relations: Vec<OsmRelation> = Vec::new();
    let mut poi_nodes: Vec<OsmPoiNode> = Vec::new();
    let mut addr_nodes: Vec<OsmPoiNode> = Vec::new();
    let mut tree_nodes: Vec<OsmNode> = Vec::new();
    let mut min_lat = f64::MAX;
    let mut min_lon = f64::MAX;
    let mut max_lat = f64::MIN;
    let mut max_lon = f64::MIN;
    let mut explicit_bounds: Option<(f64, f64, f64, f64)> = None;

    let mut in_node = false;
    let mut cur_lat = 0.0f64;
    let mut cur_lon = 0.0f64;
    let mut cur_node_tags: HashMap<String, String> = HashMap::new();

    let mut in_way = false;
    let mut current_way_id: i64 = 0;
    let mut current_tags: HashMap<String, String> = HashMap::new();
    let mut current_node_refs: Vec<i64> = Vec::new();

    let mut in_relation = false;
    let mut current_members: Vec<RelationMember> = Vec::new();

    let mut depth: usize = 0;

    // Reused scratch buffer for read_event_into — bounded by the largest
    // single XML event in the input, NOT by the file size.
    let mut buf: Vec<u8> = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                depth += 1;
                if depth > MAX_XML_DEPTH {
                    return Err(anyhow::anyhow!(
                        "XML element nesting depth {depth} exceeds limit {MAX_XML_DEPTH} at position {}",
                        reader.buffer_position()
                    ));
                }
                match e.name().as_ref() {
                    b"bounds" => {
                        if let Some(b) = parse_bounds_attrs(e) {
                            explicit_bounds = Some(b);
                        }
                    }
                    b"node" => {
                        if let Some((id, lat, lon)) = parse_node_attrs(e) {
                            min_lat = min_lat.min(lat);
                            min_lon = min_lon.min(lon);
                            max_lat = max_lat.max(lat);
                            max_lon = max_lon.max(lon);
                            nodes.insert(id, OsmNode { lat, lon });
                            in_node = true;
                            cur_lat = lat;
                            cur_lon = lon;
                            cur_node_tags.clear();
                        }
                    }
                    b"way" => {
                        in_way = true;
                        current_way_id = e
                            .attributes()
                            .flatten()
                            .find(|a| a.key.as_ref() == b"id")
                            .and_then(|a| xml_attr_parse::<i64>(&a))
                            .unwrap_or(0);
                        current_tags.clear();
                        current_node_refs.clear();
                    }
                    b"relation" => {
                        in_relation = true;
                        current_tags.clear();
                        current_members.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => match e.name().as_ref() {
                b"bounds" => {
                    if let Some(b) = parse_bounds_attrs(e) {
                        explicit_bounds = Some(b);
                    }
                }
                b"node" => {
                    if let Some((id, lat, lon)) = parse_node_attrs(e) {
                        min_lat = min_lat.min(lat);
                        min_lon = min_lon.min(lon);
                        max_lat = max_lat.max(lat);
                        max_lon = max_lon.max(lon);
                        nodes.insert(id, OsmNode { lat, lon });
                    }
                }
                b"tag" if in_node || in_way || in_relation => {
                    if let Some((k, v)) = parse_tag_attrs(e) {
                        if in_node {
                            cur_node_tags.insert(k, v);
                        } else {
                            current_tags.insert(k, v);
                        }
                    }
                }
                b"nd" if in_way => {
                    if let Some(r) = parse_nd_ref(e) {
                        current_node_refs.push(r);
                    }
                }
                b"member" if in_relation => {
                    let (mtype, mref, mrole) = parse_member_attrs(e);
                    if mtype == "way" && mref != 0 {
                        current_members.push(RelationMember {
                            way_id: mref,
                            role: mrole,
                        });
                    }
                }
                _ => {}
            },
            Ok(Event::End(ref e)) => {
                depth = depth.saturating_sub(1);
                match e.name().as_ref() {
                    b"node" if in_node => {
                        in_node = false;
                        if cur_node_tags.keys().any(|k| {
                            matches!(
                                k.as_str(),
                                "amenity" | "shop" | "tourism" | "leisure" | "historic"
                            )
                        }) {
                            poi_nodes.push(OsmPoiNode {
                                lat: cur_lat,
                                lon: cur_lon,
                                tags: cur_node_tags.clone(),
                                source: FeatureSource::Osm,
                            });
                        }
                        if cur_node_tags.contains_key("addr:housenumber") {
                            addr_nodes.push(OsmPoiNode {
                                lat: cur_lat,
                                lon: cur_lon,
                                tags: cur_node_tags.clone(),
                                source: FeatureSource::Osm,
                            });
                        }
                        if cur_node_tags.get("natural").map(|s| s.as_str()) == Some("tree") {
                            tree_nodes.push(OsmNode {
                                lat: cur_lat,
                                lon: cur_lon,
                            });
                        }
                    }
                    b"way" if in_way => {
                        in_way = false;
                        let way = OsmWay {
                            id: current_way_id,
                            tags: current_tags.clone(),
                            node_refs: current_node_refs.clone(),
                        };
                        ways.push(way);
                    }
                    b"relation" if in_relation => {
                        in_relation = false;
                        let rel_type = current_tags.get("type").map(|s| s.as_str());
                        if rel_type == Some("multipolygon") && !current_members.is_empty() {
                            relations.push(OsmRelation {
                                tags: current_tags.clone(),
                                members: current_members.clone(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "XML parse error at position {}: {e}",
                    reader.buffer_position()
                ));
            }
            _ => {}
        }
        buf.clear();
    }

    let bounds = explicit_bounds
        .or_else(|| (min_lat < f64::MAX).then_some((min_lat, min_lon, max_lat, max_lon)));

    log::info!(
        "Parsed {} nodes, {} ways, {} relations, {} POI nodes, {} address nodes, {} tree nodes (streamed XML)",
        nodes.len(),
        ways.len(),
        relations.len(),
        poi_nodes.len(),
        addr_nodes.len(),
        tree_nodes.len(),
    );

    Ok(OsmData::new(
        nodes, ways, relations, bounds, poi_nodes, addr_nodes, tree_nodes,
    ))
}

/// Parse a `.osm` XML file into `OsmData`.
pub fn parse_osm_xml(path: &Path) -> Result<OsmData> {
    let xml =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_osm_xml_str(&xml)
}

/// Detect file format by extension and dispatch to the correct parser.
/// Supports `.osm.pbf` / `.pbf` (PBF format) and `.osm` (XML format).
pub fn parse_osm_file(path: &Path) -> Result<OsmData> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "pbf" => parse_pbf(path),
        "osm" => parse_osm_xml(path),
        other => Err(anyhow::anyhow!(
            "unsupported file format '.{other}'; expected .osm.pbf or .osm"
        )),
    }
}

fn escape_xml_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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
/// let data = OsmData::new(
///     HashMap::from([(1, OsmNode { lat: 51.5, lon: -0.10 })]),
///     Vec::new(),
///     Vec::new(),
///     None,
///     Vec::new(),
///     Vec::new(),
///     Vec::new(),
/// );
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(data.relations[0].members[0].way_id, 100);
        assert_eq!(data.relations[0].members[0].role, "outer");
        assert_eq!(data.relations[0].tags["landuse"], "park");
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
        assert!(xml.contains("<relation id=\"-7000000000\">"));

        let parsed = parse_osm_xml_str(&xml).unwrap();

        assert_eq!(parsed.relations.len(), 1);
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
}
