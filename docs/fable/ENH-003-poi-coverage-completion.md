# ENH-003 — POI Coverage Completion (`man_made` / `natural`)

> Status: done (0.4.0, 2026-07-19) · Effort: Low (hours) · Impact: stops discarding data the Overpass query already fetches
> **Requires maintainer sign-off before implementation: downstream POI counts will change.**
> Prerequisite: ARC-105 (single `POI_TAG_KEYS` constant) must be done first — this plan
> extends that constant's mechanism. QA-102's unified parser makes the change single-site.

## Goal

Make the classification layer honor what the query layer pays for: nodes fetched as
always-included POIs (`man_made` ∈ {tower, water_tower, chimney}, `natural` ∈
{peak, rock, spring}) should land in `poi_nodes` with their tags, instead of being
demoted to untagged plain nodes.

## Current State (verified at commit `55187f4`, per AUDIT ARC-105)

- The Overpass query (`src/overpass.rs:144-155`) explicitly fetches those
  `man_made`/`natural` nodes as POIs.
- The parsers (`src/osm/xml_parse.rs` both loops pre-QA-102; `src/osm/pbf.rs:48-53`)
  classify POIs by key presence over `amenity|shop|tourism|leisure|historic` only —
  the fetched `man_made`/`natural` nodes fall through to the plain `nodes` map and
  their tags are dropped.
- `poi_category` in the dedupe (`src/sources.rs:140-149`) ALREADY treats `man_made`
  as a category — the layers disagree today.
- ⚠️ Critical subtlety: `natural=tree` is classified into `tree_nodes` by a separate
  branch, and `natural` has many values that are NOT POIs (`water`, `wood`, …).
  **Key-presence classification is wrong for `natural` — value filtering is required.**

## Implementation Steps

1. **Get the maintainer decision** (this is the gate): confirm the target value sets —
   proposal: `man_made` ∈ {`tower`, `water_tower`, `chimney`}, `natural` ∈ {`peak`,
   `rock`, `spring`} — exactly the sets the Overpass query names. Any node with those
   key=value pairs becomes a POI; all other `man_made`/`natural` values keep today's
   behavior.
2. Extend the ARC-105 constant into a value-aware table in `src/osm/model.rs`:

   ```rust
   /// POI classification: key plus optional allowed-value restriction.
   /// `None` = any value of the key marks a POI (legacy five keys).
   pub(crate) const POI_TAG_RULES: &[(&str, Option<&[&str]>)] = &[
       ("amenity", None), ("shop", None), ("tourism", None),
       ("leisure", None), ("historic", None),
       ("man_made", Some(&["tower", "water_tower", "chimney"])),
       ("natural", Some(&["peak", "rock", "spring"])),
   ];
   ```

   Keep `POI_TAG_KEYS` if other call sites (the query builder comment, `poi_category`)
   still want the bare key list, or derive it. Do not export publicly.
3. In the unified parser engine (post-QA-102) and `pbf.rs`, replace the key-presence
   check with a rule check: a node is a POI if any rule matches
   (`tags.get(key).is_some_and(|v| allowed.is_none_or(|vals| vals.contains(&v.as_str())))`).
   **Ordering constraint**: the `natural=tree` → `tree_nodes` branch must be evaluated
   BEFORE the POI rules (read the current branch order first; preserve tree, addr, and
   POI precedence exactly except for the intended new POI matches).
4. Update `poi_category` (`src/sources.rs`) so `natural` is also a recognized category
   key (it already handles `man_made`) — read the function; keep its category-priority
   ordering stable, appending `natural` last.
5. Tests:
   - Parser: `man_made=tower` node → in `poi_nodes` with tags; `man_made=pier` node →
     plain node (value filtering works); `natural=peak` → POI; `natural=tree` → STILL
     `tree_nodes` (not POI); `natural=water` → plain.
   - Same five cases through the PBF path once ENH-002's fixture exists (extend the
     fixture) or via the XML path only if ENH-002 is not yet done.
   - Dedupe: a `natural=peak` OSM POI and a nearby Overture POI dedupe according to
     the existing mode semantics (one test, reuse existing dedupe test shapes).
6. Documentation + versioning:
   - CHANGELOG Unreleased/Changed: "man_made (tower/water_tower/chimney) and natural
     (peak/rock/spring) nodes are now classified as POIs — poi_nodes counts increase."
   - Update the ARC-105 comment at the query builder (the "intentionally over-fetches"
     note becomes obsolete — the layers now agree).
   - This is a behavior change, not an API break: minor version (0.2.x → 0.3.0 if
     batched with ENH-007, else 0.2.2 with a prominent changelog entry — maintainer's
     call).

## Files to Touch

- `src/osm/model.rs` (rule table)
- `src/osm/xml_parse.rs` (unified engine classification)
- `src/osm/pbf.rs` (same classification)
- `src/sources.rs` (`poi_category`)
- `CHANGELOG.md`
- Tests in the parser modules (+ `tests/fixtures/` if ENH-002 landed)

## Verification

```bash
cargo test xml_parse && cargo test pbf && cargo test sources
cargo test --no-default-features
make checkall
```

## Rollback

Remove the two new rows from `POI_TAG_RULES` (single site post-ARC-105) — classification
reverts to the legacy five keys; delete the added tests and changelog line. No cache
invalidation needed: cached Overpass XML re-parses under whatever rules are active.
