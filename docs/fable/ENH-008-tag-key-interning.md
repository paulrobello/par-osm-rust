# ENH-008 — Tag-Storage Allocation Reduction (Key Interning)

> Status: proposed · Effort: High · Impact: per-tag allocation eliminated for hot keys; lower RSS on large extracts
> **Gated twice**: (1) it changes the public type of every `tags` field → 0.3.0-class,
> sequence inside/after ENH-007; (2) it must be justified by measurement first —
> implement ENH-006 (bench harness) and run the baseline profile below BEFORE writing
> any interning code. If the measured win is <10% on parse allocations, reject this
> idea and mark it rejected in ENHANCEMENTS.md with the numbers.

## Goal

Stop allocating a fresh `String` for every tag key during parsing. OSM tag keys come
from a tiny hot vocabulary (`highway`, `building`, `name`, `amenity`, `natural`,
`surface`, …) repeated millions of times in large extracts; interning keys as
`Arc<str>` makes the per-occurrence cost a refcount bump instead of a heap allocation,
and shrinks resident memory (one shared allocation per distinct key).

## Current State (verified at commit `55187f4`)

- Tags are `HashMap<String, String>` on `OsmWay`, `OsmPoiNode`, `OsmRelation`,
  `OsmAddrNode` (verify the full list: `grep -rn "tags: HashMap" src/`), populated by
  the XML engine, `pbf.rs`, and `overture/{parse,theme}.rs` (`map_tags_for_theme`
  builds them from `&'static str` inserts — those `.into()` calls each allocate).
- Values are NOT worth interning (mostly unique: names, numbers) — keys only.
- Public API exposes these maps directly (fields/accessors), so the key type is part
  of the API contract.

## Measurement Gate (do this first)

1. Add a dev-only heap profile: `dhat` as a dev-dependency behind a
   `#[cfg(feature = "dhat-heap")]` harness in a bench-like example, OR simply count
   allocations with a counting global allocator in a `#[cfg(test)]` harness (~40
   lines, no new deps — prefer this).
2. Baseline: parse the largest fixture available (generate a synthetic 100k-way XML
   with the existing bench fixture builder in `benches/parse_osm_xml.rs` — read it)
   and record: total allocations, bytes, wall time.
3. Compute the key-attributable share: instrument or estimate
   (#tags × avg-key-len). Proceed only if keys are ≥10% of parse allocations.

## Implementation Steps (after the gate and inside the ENH-007 branch)

1. Introduce the alias and interner in `src/osm/model.rs` (or `src/tags.rs`):

   ```rust
   pub type TagKey = std::sync::Arc<str>;
   pub type TagMap = std::collections::HashMap<TagKey, String>;

   /// Parse-scoped key interner. Pre-seeded with the hot vocabulary; unseen keys
   /// intern on first sight. NOT global — one per parse, dropped with it.
   pub(crate) struct KeyInterner(std::collections::HashSet<TagKey>);
   impl KeyInterner {
       pub(crate) fn with_common() -> Self { /* seed from COMMON_TAG_KEYS */ }
       pub(crate) fn intern(&mut self, k: &str) -> TagKey {
           match self.0.get(k) { Some(a) => a.clone(), None => { let a: TagKey = k.into(); self.0.insert(a.clone()); a } }
       }
   }
   const COMMON_TAG_KEYS: &[&str] = &[
       "highway", "building", "name", "amenity", "natural", "surface", "shop",
       "tourism", "leisure", "historic", "landuse", "waterway", "water",
       "addr:housenumber", "addr:street", "addr:city", "addr:postcode",
       "building:height", "building:levels", "bridge", "tunnel", "oneway", "ref",
   ];
   ```

   ⚠️ `HashSet<Arc<str>>::get(&str)` works because `Arc<str>: Borrow<str>` — no
   wrapper type needed.
2. Migrate the struct fields to `TagMap` (public type change — this is the breaking
   part; ENH-007's migration guide gets a section: "tags maps now key by `Arc<str>`;
   `tags.get("name")` and iteration are source-compatible; only code that constructed
   maps or matched on `&String` keys changes").
3. Thread a `&mut KeyInterner` through the XML engine, `pbf.rs`, and
   `overture/parse.rs` parse paths (same threading shape as ARC-101's allocator — one
   per parse). `map_tags_for_theme`'s `&'static str` inserts become
   `interner.intern("building")` — or, cheaper, pre-interned statics via
   `Arc<str>::from` once per parse in the interner seed (the seed list above already
   covers them; extend `COMMON_TAG_KEYS` with every literal key in `theme.rs` —
   enumerate with `grep -oE '"[a-z:_]+"\.into\(\)' src/overture/theme.rs`).
4. Consumers: `grep -rn "\.tags" src/ tests/ benches/` — most uses are `.get(str)` /
   iteration and compile unchanged. Fix constructors in tests/benches mechanically.
5. Re-run the measurement harness: record allocations/bytes/time delta in the PR
   description and in ENHANCEMENTS.md next to this entry. Regression on small inputs
   >2% wall time → investigate (interner hash overhead) before accepting; the
   `HashSet` lookup must be cheaper than the allocation it replaces.
6. Full gate + benches vs baseline.

## Files to Touch

- `src/osm/model.rs` (types, interner), `src/osm/xml_parse.rs`, `src/osm/pbf.rs`,
  `src/overture/parse.rs`, `src/overture/theme.rs`, plus mechanical test/bench
  constructor updates crate-wide.

## Verification

```bash
make checkall
cargo test --no-default-features
cargo bench --bench parse_osm_xml --bench merge_source_data   # vs saved baselines
# + the allocation-count harness numbers, before vs after, pasted into the PR
```

## Rollback

Pre-release: revert the branch commits (the alias confines most churn). Post-release:
reverting is another breaking change — which is exactly why the measurement gate and
the ENH-007 batching exist. If the gate fails, mark this entry rejected in
ENHANCEMENTS.md with the measured numbers so it is not re-proposed.
