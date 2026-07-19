//! Centralized synthetic-ID allocation for `par_osm_rust`.
//!
//! Several code paths synthesize OSM IDs that have no real OSM counterpart:
//!
//! - The OSM XML writer ([`crate::osm::write_osm_xml_string`]) emits POI,
//!   address, and tree nodes plus fallback way/relation IDs when the source
//!   [`crate::osm::OsmData`] does not carry one.
//! - Overture Maps GeoJSON normalization
//!   ([`crate::overture::parse_overture_geojson`]) assigns IDs to every
//!   node and way it produces, because Overture features do not carry OSM
//!   IDs.
//!
//! # Non-overlap contract
//!
//! Real OSM IDs are always non-negative, so every synthetic ID is kept
//! strictly negative. The four independent allocators are assigned distinct
//! base offsets on the negative number line, and each allocator only ever
//! decrements (issues IDs that grow strictly more negative). The bases are
//! ordered from most-negative (writer nodes) to least-negative (Overture):
//!
//! ```text
//!   writer node   writer way   writer relation   overture    0   real OSM IDs
//!   -9_000_000_000  -8_000_000_000  -7_000_000_000  -1_000_000_000
//!        <               <                <               <       <
//! ```
//!
//! Because every allocator starts at its base and moves strictly downward,
//! two allocators can only collide if one issues more IDs than the gap
//! between its base and the next-lower base (1 billion IDs per band). That
//! budget is far above any realistic crate workload, and the base ordering
//! itself is checked at compile time by the [`const _`] assertions at the
//! bottom of this module — adjusting a base so the ranges overlap is a
//! build error, not a silent corruption.
//!
//! # Determinism
//!
//! The Overture allocator (`OvertureIdAllocator`) follows a per-fetch
//! ownership rule: a single fetch constructs **one** allocator and threads
//! it through every per-theme parse call, so identical fetch inputs (bbox +
//! theme set + allocator base) yield identical ID sequences (ARC-009 /
//! QA-010). The previous design used a process-global `AtomicI64` whose
//! value depended on every prior parse in the process, making cache keys
//! and round-trip tests non-deterministic; the fix reset the allocator per
//! parse, but that introduced cross-theme collisions (ARC-101) because two
//! parses within one multi-theme fetch re-issued the same IDs. The current
//! contract is: **one allocator per fetch** — unique IDs within a fetch,
//! determinism across fetches.
//!
//! Note that two **independent** allocators in the same band DO collide
//! (they each start at `SYNTHETIC_OVERTURE_ID_BASE`), which is precisely
//! why fetch orchestration owns the allocator instead of letting each
//! per-theme parse construct its own. The public `parse_overture_geojson`
//! still constructs a fresh allocator for callers parsing one theme
//! standalone, preserving its ARC-009 per-call determinism contract.

use std::collections::HashSet;

/// Base offset for synthetic node IDs emitted by the OSM XML writer
/// ([`crate::osm::write_osm_xml_string`]).
pub const SYNTHETIC_NODE_ID_BASE: i64 = -9_000_000_000;

/// Base offset for synthetic way IDs emitted by the OSM XML writer when the
/// source [`crate::osm::OsmData`] does not carry a way ID.
pub const SYNTHETIC_WAY_ID_BASE: i64 = -8_000_000_000;

/// Base offset for synthetic relation IDs emitted by the OSM XML writer.
pub const SYNTHETIC_RELATION_ID_BASE: i64 = -7_000_000_000;

/// Base offset for synthetic IDs assigned by Overture GeoJSON normalization
/// ([`crate::overture::parse_overture_geojson`]).
pub const SYNTHETIC_OVERTURE_ID_BASE: i64 = -1_000_000_000;

// Compile-time proofs of the non-overlap contract. If any base is edited
// such that the ranges can collide with each other or with real (>= 0)
// OSM IDs, the build fails here.
const _: () = assert!(SYNTHETIC_NODE_ID_BASE < SYNTHETIC_WAY_ID_BASE);
const _: () = assert!(SYNTHETIC_WAY_ID_BASE < SYNTHETIC_RELATION_ID_BASE);
const _: () = assert!(SYNTHETIC_RELATION_ID_BASE < SYNTHETIC_OVERTURE_ID_BASE);
const _: () = assert!(SYNTHETIC_OVERTURE_ID_BASE < 0);

/// Deterministic allocator for Overture synthetic IDs, owned per fetch.
///
/// The counter starts at [`SYNTHETIC_OVERTURE_ID_BASE`] and decrements on
/// every allocation. There is no global state: concurrent fetches (e.g. in
/// separate threads) each own an independent allocator. There is also no
/// internal mutation guard against re-entrant use — the contract is
/// enforced by ownership:
///
/// - **Within a single fetch**, the orchestrator (e.g.
///   `crate::overture::cli::fetch_overture_data`) constructs exactly one
///   allocator and threads `&mut` it through every per-theme parse call.
///   Because a single allocator never reissues an ID, the merged ways
///   across all themes carry disjoint IDs and [`crate::osm::OsmData::merge`]
///   preserves its `ways` / `ways_by_id` invariant (ARC-101).
/// - **Across fetches**, every fetch starts a fresh allocator at
///   [`SYNTHETIC_OVERTURE_ID_BASE`], so identical fetch inputs produce
///   identical ID sequences (ARC-009 / QA-010 determinism).
/// - **Standalone parses** via the public `parse_overture_geojson` get a
///   fresh allocator per call — same per-call determinism, but a caller
///   that merges two such parses is responsible for the resulting
///   collision (two independent allocators in the same band DO collide on
///   their first ID).
///
/// IDs issued within a single allocator never collide with each other. They
/// also never collide with the writer's node/way/relation ranges (which
/// live at more-negative bases — see the module docs) or with real OSM IDs
/// (which are non-negative).
#[derive(Debug)]
pub(crate) struct OvertureIdAllocator {
    next: i64,
}

impl OvertureIdAllocator {
    /// Create a fresh allocator whose first [`Self::next_id`] call returns
    /// [`SYNTHETIC_OVERTURE_ID_BASE`].
    ///
    /// Fetch orchestrators should construct exactly one per fetch (ARC-101).
    pub(crate) fn new() -> Self {
        Self {
            next: SYNTHETIC_OVERTURE_ID_BASE,
        }
    }

    /// Returns the next synthetic ID. Each call returns a value one less
    /// than the previous call's return, so the first ID issued is
    /// [`SYNTHETIC_OVERTURE_ID_BASE`] and subsequent IDs grow strictly more
    /// negative. IDs within a single allocator never repeat.
    pub(crate) fn next_id(&mut self) -> i64 {
        let id = self.next;
        self.next -= 1;
        id
    }
}

impl Default for OvertureIdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns the next synthetic node ID for the OSM XML writer, skipping any
/// IDs already present in `occupied`.
///
/// The caller seeds `next` with [`SYNTHETIC_NODE_ID_BASE`] and accumulates
/// occupied node IDs from the source [`crate::osm::OsmData`]; this helper
/// then decrements past any collisions with pre-existing real or synthetic
/// node IDs before issuing the next one.
pub(crate) fn next_writer_node_id(next: &mut i64, occupied: &mut HashSet<i64>) -> i64 {
    while occupied.contains(next) {
        *next -= 1;
    }
    let id = *next;
    occupied.insert(id);
    *next -= 1;
    id
}

/// Returns the synthetic way ID the OSM XML writer emits for the way at
/// the given index when the source [`crate::osm::OsmData`] carries no ID
/// for it.
pub(crate) const fn writer_way_id(idx: usize) -> i64 {
    SYNTHETIC_WAY_ID_BASE - idx as i64
}

/// Returns the synthetic relation ID the OSM XML writer emits for the
/// relation at the given index.
pub(crate) const fn writer_relation_id(idx: usize) -> i64 {
    SYNTHETIC_RELATION_ID_BASE - idx as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── (a) all synthetic IDs are strictly negative ───────────────────────

    #[test]
    fn all_bases_are_strictly_negative() {
        const { assert!(SYNTHETIC_NODE_ID_BASE < 0) };
        const { assert!(SYNTHETIC_WAY_ID_BASE < 0) };
        const { assert!(SYNTHETIC_RELATION_ID_BASE < 0) };
        const { assert!(SYNTHETIC_OVERTURE_ID_BASE < 0) };
    }

    #[test]
    fn overture_allocator_only_emits_strictly_negative_ids() {
        let mut alloc = OvertureIdAllocator::new();
        for _ in 0..10_000 {
            assert!(alloc.next_id() < 0);
        }
    }

    #[test]
    fn writer_helpers_emit_strictly_negative_ids() {
        for idx in 0..1_000 {
            assert!(writer_way_id(idx) < 0);
            assert!(writer_relation_id(idx) < 0);
        }

        // Writer node allocator: skip-none path stays strictly negative.
        let mut next = SYNTHETIC_NODE_ID_BASE;
        let mut occupied = HashSet::new();
        for _ in 0..1_000 {
            let id = next_writer_node_id(&mut next, &mut occupied);
            assert!(id < 0);
        }
    }

    // ── (b) node/way/relation/overture ranges don't overlap ───────────────

    #[test]
    fn bases_are_strictly_ordered_no_overlap() {
        // Each allocator only decrements from its base, so the next-higher
        // base is the lower bound (exclusive) of the range below it. These
        // orderings are also enforced at module level via `const _`
        // assertions; the inline `const { assert!(..) }` blocks below
        // re-prove them at compile time inside the test for locality.
        const { assert!(SYNTHETIC_NODE_ID_BASE < SYNTHETIC_WAY_ID_BASE) };
        const { assert!(SYNTHETIC_WAY_ID_BASE < SYNTHETIC_RELATION_ID_BASE) };
        const { assert!(SYNTHETIC_RELATION_ID_BASE < SYNTHETIC_OVERTURE_ID_BASE) };
        const { assert!(SYNTHETIC_OVERTURE_ID_BASE < 0) };

        // Realistic-budget sanity: each band has at least 1 billion IDs of
        // headroom before reaching the next-higher band.
        let band = SYNTHETIC_WAY_ID_BASE - SYNTHETIC_NODE_ID_BASE;
        assert!(band >= 1_000_000_000);
        let band = SYNTHETIC_RELATION_ID_BASE - SYNTHETIC_WAY_ID_BASE;
        assert!(band >= 1_000_000_000);
        let band = SYNTHETIC_OVERTURE_ID_BASE - SYNTHETIC_RELATION_ID_BASE;
        assert!(band >= 1_000_000_000);
    }

    #[test]
    fn writer_way_and_relation_ranges_do_not_overlap_for_realistic_counts() {
        let ways: std::collections::HashSet<i64> = (0..1_000_000).map(writer_way_id).collect();
        let rels: std::collections::HashSet<i64> = (0..1_000_000).map(writer_relation_id).collect();
        // Disjoint by construction (way band < relation band); this loop
        // confirms no value lands in both sets.
        for id in &ways {
            assert!(
                !rels.contains(id),
                "way id {id} collides with relation range"
            );
        }
    }

    #[test]
    fn overture_range_does_not_overlap_writer_ranges() {
        let mut alloc = OvertureIdAllocator::new();
        for _ in 0..1_000_000 {
            let id = alloc.next_id();
            // Overture IDs sit above the writer relation base (-7e9) by
            // construction; this asserts the runtime invariant too.
            assert!(
                id > SYNTHETIC_RELATION_ID_BASE,
                "overture id {id} entered writer ranges"
            );
            assert!(id <= SYNTHETIC_OVERTURE_ID_BASE);
        }
    }

    // ── (c) determinism: two parses of identical input yield identical IDs ─

    #[test]
    fn overture_allocator_is_deterministic_across_instances() {
        // Two fresh allocators over the same call sequence produce the
        // same IDs — the core ARC-009 contract.
        let mut a = OvertureIdAllocator::new();
        let mut b = OvertureIdAllocator::new();
        for _ in 0..5_000 {
            assert_eq!(a.next_id(), b.next_id());
        }
    }

    #[test]
    fn overture_allocator_first_id_matches_documented_base() {
        let mut alloc = OvertureIdAllocator::new();
        assert_eq!(alloc.next_id(), SYNTHETIC_OVERTURE_ID_BASE);
    }

    // ── (d) within a single large parse no two features collide ───────────

    #[test]
    fn overture_allocator_never_repeats_an_id() {
        let mut alloc = OvertureIdAllocator::new();
        let mut seen = HashSet::new();
        for _ in 0..100_000 {
            let id = alloc.next_id();
            assert!(seen.insert(id), "duplicate synthetic id emitted: {id}");
        }
    }

    #[test]
    fn writer_node_allocator_skips_occupied_without_collision() {
        // Seed the occupied set with the allocator's first three IDs so the
        // skip-past-collisions path is exercised.
        let pre_occupied: HashSet<i64> = [
            SYNTHETIC_NODE_ID_BASE,
            SYNTHETIC_NODE_ID_BASE - 1,
            SYNTHETIC_NODE_ID_BASE - 2,
        ]
        .into_iter()
        .collect();

        let mut occupied = pre_occupied.clone();
        let mut next = SYNTHETIC_NODE_ID_BASE;
        let first = next_writer_node_id(&mut next, &mut occupied);
        // The first three IDs (-9e9, -9e9-1, -9e9-2) were pre-occupied, so
        // the first emitted id must be the one below them.
        assert_eq!(first, SYNTHETIC_NODE_ID_BASE - 3);
        assert!(
            !pre_occupied.contains(&first),
            "writer failed to skip a pre-occupied id"
        );

        // Subsequent IDs never repeat and never intersect the pre-occupied set.
        let mut emitted: HashSet<i64> = [first].into_iter().collect();
        for _ in 0..1_000 {
            let id = next_writer_node_id(&mut next, &mut occupied);
            assert!(
                !pre_occupied.contains(&id),
                "writer entered the pre-occupied band at {id}"
            );
            assert!(emitted.insert(id), "writer emitted duplicate id {id}");
        }
    }
}
