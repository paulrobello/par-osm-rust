# ENH-005 — Table-Driven `map_tags_for_theme`

> Status: done (0.5.0, 2026-07-20) · Effort: Low-Medium (~half day) · Impact: cyclomatic 26 → <10 per unit; Overture schema changes become table edits
> Independent of the audit phases (touches only `src/overture/theme.rs`); behavior is
> pinned by existing theme tests. Do after ARC-101 lands to avoid same-module churn.

## Goal

Replace the #3-most-complex function in the crate (`map_tags_for_theme`,
`src/overture/theme.rs:118`, cyclomatic 26 per par-mem) with declarative mapping
data plus a small interpreter, so each Overture→OSM mapping is one visible, testable
row instead of a branch in a Critical-complexity function.

## Current State (verified at commit `55187f4`)

`map_tags_for_theme(props: &Value, theme: OvertureTheme) -> HashMap<String, String>`
has four arms with three distinct shapes:

- **Building / Transportation / Place** — regular field mappings:
  - str field → tag with optional default (`class`→`building` default `"yes"`;
    `class`→`highway` default `"unclassified"`)
  - f64/u64 field → stringified tag (`height`→`building:height`, `num_floors`→`building:levels`)
  - bool flag → constant tag (`is_bridge`→`bridge=yes`, `is_tunnel`→`tunnel=yes`)
  - nested path → tag (`names.primary`→`name`)
  - categorized: `categories.primary` → key chosen by `map_place_category_to_osm_key`
    (already a separate helper, complexity fine — keep it)
- **Base** — a subtype classifier: `matches!` groups over `subtype` (water bodies,
  waterways, landuse groups, …) each inserting one or two fixed-or-derived tags.
  (Read the full arm — the excerpt above line 229 continues; enumerate every group
  before refactoring.)

## Implementation Steps

1. Read `src/overture/theme.rs` in full, including its test module — the tests define
   the behavior contract; do not touch them (except to add).
2. Define the rule vocabulary (private to the module):

   ```rust
   enum Rule {
       /// props[src] as str → tags[dst]; None default = omit when missing.
       Str { src: &'static str, dst: &'static str, default: Option<&'static str> },
       /// props[src] as f64 → tags[dst] (Display-formatted, matching current output).
       F64 { src: &'static str, dst: &'static str },
       /// props[src] as u64 → tags[dst].
       U64 { src: &'static str, dst: &'static str },
       /// props[src] as bool(true) → tags[dst] = val.
       Flag { src: &'static str, dst: &'static str, val: &'static str },
       /// props[a][b] as str → tags[dst] (the names.primary shape).
       Nested2 { a: &'static str, b: &'static str, dst: &'static str },
   }
   ```

   ⚠️ Number formatting: the current code uses `h.to_string()` on `f64` — the table
   interpreter must produce byte-identical strings (`to_string`, not `format!("{:.1}")`).
3. Per-theme rule tables:

   ```rust
   const BUILDING_RULES: &[Rule] = &[
       Rule::Str { src: "class", dst: "building", default: Some("yes") },
       Rule::F64 { src: "height", dst: "building:height" },
       Rule::U64 { src: "num_floors", dst: "building:levels" },
   ];
   const TRANSPORTATION_RULES: &[Rule] = &[ /* class/highway default unclassified,
       Nested2 names/primary/name, Str road_surface/surface, Flag is_bridge, Flag is_tunnel */ ];
   ```

   Order rows to match the current insertion order (HashMap output is order-free, but
   keep the source readable in the old order for diffability).
4. One interpreter: `fn apply_rules(props: &Value, rules: &[Rule], tags: &mut HashMap<String, String>)`
   — a single `for` + `match` over the five variants.
5. `Place`: express `names.primary` via the table; keep the `categories.primary` +
   `map_place_category_to_osm_key` logic as 5 explicit lines after `apply_rules`
   (a bespoke rule variant for one use is over-engineering — leave it direct).
6. `Base`: extract to `fn map_base_tags(subtype: &str, class: &str, tags: &mut ...)`
   with a group table:

   ```rust
   const BASE_SUBTYPE_GROUPS: &[(&[&str], BaseAction)] = ...
   ```

   ONLY if the remaining groups share a uniform shape — read all of them first. If
   the groups are irregular (some use `class`, some insert two tags conditionally),
   the honest refactor is: keep the `match`/`matches!` chain but in the extracted
   `map_base_tags` function — that alone drops `map_tags_for_theme` below 10 and
   `map_base_tags` lands ~12. Prefer honest extraction over forcing a table that
   obscures irregularity.
7. `map_tags_for_theme` becomes: dispatch theme → `apply_rules(theme_rules)` (+ the
   Place category block / `map_base_tags` call). Target: each function ≤ 10.
8. Tests: existing theme tests must pass byte-identically. Add one table-coverage
   test per theme asserting every rule row fires (a props JSON exercising all fields
   at once → expected full tag map).
9. Run gate + `cargo test --no-default-features` (theme.rs is in the pure subset).

## Files to Touch

- `src/overture/theme.rs` only.

## Verification

```bash
cargo test overture
cargo test --no-default-features
make checkall
```

Optional: re-run par-mem `find_most_complex_functions` after reindex — the two theme
functions should leave the Critical band.

## Rollback

Single-file, behavior-identical refactor: revert the commit. No API or data change.
