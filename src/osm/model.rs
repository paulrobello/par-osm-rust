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

/// Tag keys whose presence on a standalone OSM node classifies it as a POI.
///
/// This is the **single source of truth** for runtime POI classification
/// (ARC-105): the XML parser ([`super::xml_parse`]) and the PBF parser
/// ([`super::pbf`]) both iterate this constant when deciding whether a node
/// belongs in [`OsmData::poi_nodes`]. The dedupe helper
/// `crate::sources::poi_category` extends this list with `man_made` as a
/// dedupe-only extra category (two `man_made` POIs must not dedupe against
/// each other across categories); runtime classification stays at these
/// five keys so `man_made`/`natural` nodes intentionally over-fetched by
/// `crate::overpass::build_overpass_query` are NOT silently promoted to
/// POIs — see the ARC-105 comment in that function.
pub(crate) const POI_TAG_KEYS: &[&str] = &["amenity", "shop", "tourism", "leisure", "historic"];

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
    /// The relation's own OSM identifier (ARC-113, 0.3.0). Mirrors
    /// [`OsmWay::id`]: populated by the parsers from the `<relation id="…">`
    /// attribute (XML) or the PBF `Relation::id` field; emitted by
    /// [`crate::osm::write_osm_xml_string`] when present, with a synthetic
    /// fallback for id-less synthetic relations.
    pub id: i64,
    /// Free-form OSM tags on the relation (must include `type=multipolygon`
    /// for the parser to retain it).
    pub tags: HashMap<String, String>,
    /// Ways belonging to the relation, each with its role.
    pub members: Vec<RelationMember>,
}

/// Parsed OSM dataset.
///
/// **Field encapsulation (ARC-109, 0.3.0).** Every collection on this
/// struct is `pub(crate)`; downstream consumers read them through the
/// accessors below ([`OsmData::nodes`], [`OsmData::ways`],
/// [`OsmData::ways_by_id`], [`OsmData::relations`], [`OsmData::bounds`],
/// [`OsmData::poi_nodes`], [`OsmData::addr_nodes`], [`OsmData::tree_nodes`],
/// plus [`OsmData::iter_ways`] and [`OsmData::way_id_at`]). Construct an
/// `OsmData` via [`OsmData::default`] plus the consuming `with_*` builder
/// methods ([`OsmData::with_nodes`], [`OsmData::with_ways`], …). The
/// historical [`OsmData::new`] constructor is retained through the 0.3.x
/// deprecation window but emits a `deprecated` warning per call.
///
/// The `ways` / `ways_by_id` pair in particular must stay in lock-step: each
/// entry in `ways` has exactly one corresponding entry in `ways_by_id`
/// mapping its OSM id to its index. The pair is mutated only by
/// [`OsmData::default`]/[`OsmData::with_ways`] (the builder route),
/// [`OsmData::new`] (the deprecated route), and [`OsmData::push_way`];
/// in-place bulk operations (`merge`, `clip_to_bbox`) preserve the
/// invariant internally and are checked by [`OsmData::validate_invariants`]
/// in debug builds.
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
    /// exclusively by the constructor paths and [`OsmData::push_way`].
    pub(crate) ways_by_id: HashMap<i64, usize>,
    /// Multipolygon relations.
    pub(crate) relations: Vec<OsmRelation>,
    /// Bounding box: (south, west, north, east)
    pub(crate) bounds: Option<(f64, f64, f64, f64)>,
    /// Standalone nodes with POI tags (amenity, shop, tourism, leisure, historic).
    pub(crate) poi_nodes: Vec<OsmPoiNode>,
    /// Standalone nodes with address tags (addr:housenumber).
    /// These are typically entrance/door nodes placed on building outlines in OSM.
    pub(crate) addr_nodes: Vec<OsmPoiNode>,
    /// Individual tree positions (from OSM `natural=tree` or Overture `land/tree`).
    pub(crate) tree_nodes: Vec<OsmNode>,
}

impl Default for OsmData {
    /// Empty `OsmData` — the starting point for the `with_*` builder.
    ///
    /// All collections are empty and `bounds` is `None`. The `ways` /
    /// `ways_by_id` invariant holds trivially (both empty). Prefer this
    /// plus the `with_*` chain over the deprecated [`OsmData::new`]
    /// constructor (ARC-109, 0.3.0).
    fn default() -> Self {
        Self {
            nodes: HashMap::new(),
            ways: Vec::new(),
            ways_by_id: HashMap::new(),
            relations: Vec::new(),
            bounds: None,
            poi_nodes: Vec::new(),
            addr_nodes: Vec::new(),
            tree_nodes: Vec::new(),
        }
    }
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
    /// **Deprecated since 0.3.0 (ARC-109).** Prefer
    /// [`OsmData::default`] plus the `with_*` builder, which composes more
    /// naturally and routes way insertion through the same invariant-maintaining
    /// path. The historical positional-argument constructor is retained for
    /// the 0.3.x deprecation window to keep downstream call sites compiling.
    ///
    /// # Examples
    ///
    /// ```
    /// # #[allow(deprecated)]
    /// # {
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
    /// # }
    /// ```
    #[deprecated(
        since = "0.3.0",
        note = "use OsmData::default() + the with_* builder"
    )]
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

    // ── Builder (ARC-109, 0.3.0) ──────────────────────────────────────────
    //
    // Each `with_*` method consumes `self`, sets one field, and returns
    // `Self` so callers can chain:
    //
    //     let data = OsmData::default()
    //         .with_nodes(nodes)
    //         .with_ways(ways)
    //         .with_bounds(Some(bbox));
    //
    // `with_ways` is the only method that touches more than one field — it
    // also rebuilds `ways_by_id` from `OsmWay::id`, exactly as the deprecated
    // `new()` constructor does. Every other `with_*` is a plain field
    // assignment. Every method re-validates the invariants in debug builds so
    // a misconfigured builder chain fails loudly.

    /// Replace the `nodes` map (keyed by OSM id).
    pub fn with_nodes(mut self, nodes: HashMap<i64, OsmNode>) -> Self {
        self.nodes = nodes;
        debug_assert!(
            self.validate_invariants().is_ok(),
            "OsmData::with_nodes produced an inconsistent state"
        );
        self
    }

    /// Replace the `ways` slice and rebuild `ways_by_id` from each
    /// [`OsmWay::id`], preserving the `ways` / `ways_by_id` invariant. This
    /// is the sanctioned builder route for the invariant that the deprecated
    /// [`OsmData::new`] constructor also establishes.
    pub fn with_ways(mut self, ways: Vec<OsmWay>) -> Self {
        self.ways_by_id = ways
            .iter()
            .enumerate()
            .map(|(idx, way)| (way.id, idx))
            .collect();
        self.ways = ways;
        debug_assert!(
            self.validate_invariants().is_ok(),
            "OsmData::with_ways produced an inconsistent state"
        );
        self
    }

    /// Replace the multipolygon relations slice.
    pub fn with_relations(mut self, relations: Vec<OsmRelation>) -> Self {
        self.relations = relations;
        debug_assert!(
            self.validate_invariants().is_ok(),
            "OsmData::with_relations produced an inconsistent state"
        );
        self
    }

    /// Replace the dataset bounding box (`(south, west, north, east)`).
    pub fn with_bounds(mut self, bounds: Option<(f64, f64, f64, f64)>) -> Self {
        self.bounds = bounds;
        debug_assert!(
            self.validate_invariants().is_ok(),
            "OsmData::with_bounds produced an inconsistent state"
        );
        self
    }

    /// Replace the standalone POI nodes slice.
    pub fn with_poi_nodes(mut self, poi_nodes: Vec<OsmPoiNode>) -> Self {
        self.poi_nodes = poi_nodes;
        debug_assert!(
            self.validate_invariants().is_ok(),
            "OsmData::with_poi_nodes produced an inconsistent state"
        );
        self
    }

    /// Replace the standalone address nodes slice.
    pub fn with_addr_nodes(mut self, addr_nodes: Vec<OsmPoiNode>) -> Self {
        self.addr_nodes = addr_nodes;
        debug_assert!(
            self.validate_invariants().is_ok(),
            "OsmData::with_addr_nodes produced an inconsistent state"
        );
        self
    }

    /// Replace the standalone tree nodes slice.
    pub fn with_tree_nodes(mut self, tree_nodes: Vec<OsmNode>) -> Self {
        self.tree_nodes = tree_nodes;
        debug_assert!(
            self.validate_invariants().is_ok(),
            "OsmData::with_tree_nodes produced an inconsistent state"
        );
        self
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

    /// Borrow the multipolygon relations slice.
    ///
    /// Relations are stored in arrival order (parser) or appended in
    /// merge order; no de-duplication is performed.
    pub fn relations(&self) -> &[OsmRelation] {
        &self.relations
    }

    /// The dataset bounding box as `(south, west, north, east)`, or `None`
    /// when no bbox has been recorded (e.g. an empty dataset, or one
    /// constructed via [`OsmData::default`] without [`OsmData::with_bounds`]).
    pub fn bounds(&self) -> Option<(f64, f64, f64, f64)> {
        self.bounds
    }

    /// Borrow the standalone POI nodes slice (nodes carrying
    /// `amenity`/`shop`/`tourism`/`leisure`/`historic`).
    pub fn poi_nodes(&self) -> &[OsmPoiNode] {
        &self.poi_nodes
    }

    /// Borrow the standalone address nodes slice (nodes carrying
    /// `addr:housenumber`, typically entrance/door nodes on building
    /// outlines).
    pub fn addr_nodes(&self) -> &[OsmPoiNode] {
        &self.addr_nodes
    }

    /// Borrow the individual tree-node positions (OSM `natural=tree` or
    /// Overture `land/tree`). Each entry carries lat/lon only — no tags.
    pub fn tree_nodes(&self) -> &[OsmNode] {
        &self.tree_nodes
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
    ///   `other` keeps `other`'s value. Callers merging synthetic-Overture
    ///   data must thread one `OvertureIdAllocator` per fetch so cross-theme
    ///   parses mint disjoint IDs (ARC-101); merging two independently
    ///   allocated Overture results remains the caller's responsibility.
    /// * `ways` — `other`'s ways are appended in order, **skipping** any whose
    ///   ID is already present in `self.ways_by_id` (first-wins; a
    ///   `log::warn!` is emitted per skip). The skip-first-wins rule keeps
    ///   the `ways` / `ways_by_id` invariant intact when a caller merges two
    ///   [`OsmData`]s whose way IDs were not produced by a single allocator
    ///   (e.g. an external caller merging two independently-fetched Overture
    ///   results). Within a single multi-theme fetch the per-fetch
    ///   `OvertureIdAllocator` (ARC-101) makes the skip arm unreachable.
    ///   Indices for appended ways are shifted by `self.ways.len()` so each
    ///   `(id → index)` entry points at the right slot.
    /// * `relations` — `other`'s relations are appended; no de-duplication.
    /// * `poi_nodes`, `addr_nodes`, `tree_nodes` — `other`'s entries are
    ///   appended in order; no de-duplication.
    /// * `bounds` — when both sides have a bbox, the per-axis union is stored
    ///   `(min(south), min(west), max(north), max(east))`. When only
    ///   one side has a bbox, that bbox is kept. When neither side has one,
    ///   `bounds` remain `None`.
    ///
    /// Like [`OsmData::clip_to_bbox`], this method ends with a
    /// `debug_assert!(self.validate_invariants().is_ok(), ...)` so the
    /// `ways` / `ways_by_id` invariant is checked in debug builds after
    /// every merge (ARC-103).
    pub fn merge(&mut self, other: OsmData) {
        self.nodes.extend(other.nodes);
        for way in other.ways {
            if self.ways_by_id.contains_key(&way.id) {
                log::warn!("OsmData::merge: skipping way with duplicate id {}", way.id);
                continue;
            }
            let idx = self.ways.len();
            self.ways_by_id.insert(way.id, idx);
            self.ways.push(way);
        }
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

        debug_assert!(
            self.validate_invariants().is_ok(),
            "OsmData::merge produced an inconsistent state"
        );
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

        // Filter ways: keep if any node is inside the bbox. QA-106: take
        // `self.ways` up front (moves the Vec out, leaving an empty Vec behind)
        // so the borrow checker allows the `self.nodes.get(...)` lookup inside
        // the loop body — iterating `&self.ways` while borrowing `self.nodes`
        // is the constraint that forced the per-way `way.clone()` before. With
        // the Vec owned locally, survivors move into `kept_ways` with no clone.
        let ways = std::mem::take(&mut self.ways);
        let mut keep_node_ids: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let mut kept_ways: Vec<OsmWay> = Vec::new();
        for way in ways {
            let touches_bbox = way
                .node_refs
                .iter()
                .any(|id| self.nodes.get(id).is_some_and(|n| in_bbox(n.lat, n.lon)));
            if touches_bbox {
                for id in &way.node_refs {
                    keep_node_ids.insert(*id);
                }
                kept_ways.push(way);
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
