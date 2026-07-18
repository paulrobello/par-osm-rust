//! Data model for parsed OpenStreetMap datasets.
//!
//! This submodule owns the [`OsmData`] aggregate plus the value types it
//! collections hold ([`OsmNode`], [`OsmWay`], [`OsmRelation`],
//! [`RelationMember`], [`OsmPoiNode`], [`FeatureSource`]). The parsers in
//! [`super::pbf`] and [`super::xml_parse`] construct an [`OsmData`]; the
//! serializer in [`super::xml_write`] reads one back out.
//!
//! See the crate-level [`osm`](crate::osm) module docs for the high-level
//! data-flow.

use std::collections::{HashMap, HashSet};

/// A geographic point from the OSM dataset.
#[derive(Debug, Clone, Copy)]
pub struct OsmNode {
    /// Latitude in decimal degrees (WGS-84).
    pub lat: f64,
    /// Longitude in decimal degrees (WGS-84).
    pub lon: f64,
}

/// Data source for normalized map features.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureSource {
    /// Feature originated from OpenStreetMap (parsed XML/PBF or an
    /// OSM-conformant mirror like Overpass).
    #[default]
    Osm,
    /// Feature originated from Overture Maps (normalized GeoJSON).
    Overture,
    /// Feature was synthesized by the crate itself (no real-world
    /// counterpart) — e.g. allocator-issued synthetic IDs from
    /// [`crate::synthetic_ids`].
    Synthetic,
}

/// An OSM node that carries feature tags (amenity, shop, tourism, etc.).
/// Used for POI marker placement.
#[derive(Debug, Clone)]
pub struct OsmPoiNode {
    /// Latitude in decimal degrees (WGS-84).
    pub lat: f64,
    /// Longitude in decimal degrees (WGS-84).
    pub lon: f64,
    /// Free-form OSM tags on the node (`amenity`, `shop`, `name`, …).
    pub tags: HashMap<String, String>,
    /// Provenance of this POI — OSM, Overture, or synthetic.
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
    /// The way's own OSM identifier (the single source of truth; QA-021).
    pub id: i64,
    /// Free-form OSM tags on the way (`highway`, `building`, `name`, …).
    pub tags: HashMap<String, String>,
    /// Ordered list of OSM node IDs referenced by the way. Positions are
    /// resolved against the parent [`OsmData`] node map by consumers
    /// (parser, writer, clip), not at parse time.
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
    /// Free-form OSM tags on the relation (must include `type=multipolygon`
    /// for the parser to retain it).
    pub tags: HashMap<String, String>,
    /// Ways belonging to the relation, each with its role.
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
    /// Bounding box: (south, west, north, east)
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

    /// Borrow the node map keyed by OSM id.
    ///
    /// Downstream consumers resolve way node references (terrain, geometry,
    /// sign placement) through this map. Exposed as a read view so the field's
    /// `pub(crate)` encapsulation does not block id lookups.
    pub fn nodes(&self) -> &HashMap<i64, OsmNode> {
        &self.nodes
    }

    /// Borrow the ways slice in insertion order.
    ///
    /// Index `i` here is the same index stored in [`OsmData::ways_by_id`] and
    /// yielded positionally by [`OsmData::iter_ways`].
    pub fn ways(&self) -> &[OsmWay] {
        &self.ways
    }

    /// Borrow the way-id to ways-index lookup map.
    ///
    /// Used to resolve multipolygon relation members to their way position in
    /// [`OsmData::ways`].
    pub fn ways_by_id(&self) -> &HashMap<i64, usize> {
        &self.ways_by_id
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
    ///   `(min(south), min(west), max(north), max(east))`. When only
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
    /// `bbox` is `(south, west, north, east)`.
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
