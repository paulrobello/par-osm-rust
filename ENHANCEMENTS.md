# Enhancement Ideas — par-osm-rust

> **Generated**: 2026-07-18 (Fable audit cycle, commit `55187f4`, v0.2.1)
> **Scope**: opportunities beyond the defect findings in `AUDIT.md` — performance,
> coverage, API evolution, and maintainability investments.
> **Per-idea implementation plans**: `docs/fable/ENH-XXX-<slug>.md` — each written to be
> executable by a smaller model without further analysis.
>
> **Maintenance rule**: when an idea is implemented, mark it done here (change its
> checkbox to `[x]` and append `— done <date>, <commit>`). Do not delete completed
> entries; they document why the change exists. Remove an idea only if it is rejected,
> and note why.
>
> Graph evidence sources (par-mem, repo_id `par-osm-rust`): `find_most_complex_functions`
> (the two XML parsers rank #1/#2 at cyclomatic 41/40; `map_tags_for_theme` #3 at 26),
> `find_dead_code` (confirms `priority_for` has zero callers — ARC-102), repository stats
> (338 functions, 625 call edges, 24 Rust files). Centrality/hotspot analytics were
> unavailable (index analytics did not complete during the audit window).

---

## Priority Order

| # | ID | Title | Impact | Effort | Status |
| --- | ---- | ------- | -------- | -------- | -------- |
| 1 | ENH-001 | Parallel SRTM tile downloads | High (wall-clock) | Medium | [x] — done 2026-07-19, commit TBD |
| 2 | ENH-002 | PBF test fixture + format coverage | High (coverage) | Low-Medium | [x] — done 2026-07-19 |
| 3 | ENH-003 | POI coverage completion (`man_made`/`natural`) | Medium (data value) | Low | [ ] |
| 4 | ENH-004 | Streaming Overpass fetch→parse pipeline | High (memory) | Medium | [ ] |
| 5 | ENH-005 | Table-driven `map_tags_for_theme` | Medium (maintainability) | Low-Medium | [ ] |
| 6 | ENH-006 | Criterion benchmark regression tracking in CI | Medium (guardrail) | Medium | [ ] |
| 7 | ENH-007 | 0.3.0 API modernization release plan | High (API quality) | High | [ ] |
| 8 | ENH-008 | Tag-storage allocation reduction (key interning) | Medium (perf/memory) | High | [ ] |

---

## ENH-001 — Parallel SRTM tile downloads

`srtm::download_tiles_for_bbox` downloads tiles strictly sequentially, with in-loop
retry/backoff sleeps blocking the whole batch (`src/srtm.rs:195-240`; flagged as the
non-defect half of ARC-114). SRTM1 tiles are ~25 MB compressed and a modest regional
bbox spans dozens of tiles, so elevation-heavy consumers (osm-to-bedrock terrain
builds) pay minutes of serial wall-clock that a small bounded worker pool would cut
roughly by the pool width. The crate already has no async runtime and should not grow
one for this — `std::thread::scope` with a bounded worker count (default 4) preserves
the blocking API, the retry semantics, and the progress-callback contract while
overlapping network waits.

**Expected impact**: 3–5× faster multi-tile elevation fetches (network-bound).
**Effort**: Medium — ~1 day including tests. **Plan**: `docs/fable/ENH-001-parallel-srtm-downloads.md`

## ENH-002 — PBF test fixture + format coverage

`parse_pbf` is an entire public input format with **zero test coverage** — the audit
found no `.pbf` fixture anywhere in the repo; only `parse_osm_file`'s format-dispatch
and error branches are tested (QA coverage gap). The fix is a tiny checked-in fixture
(a few nodes/ways/relations with POI/addr/tree tags, generated once by a small
committed generator script so it can be regenerated) plus integration tests asserting
parity with the equivalent XML parse. This also protects the QA-101 duplicate-ID policy
and any future ARC-105 key-list changes on the PBF path, which currently ship blind.

**Expected impact**: closes the largest coverage blind spot; enables safe refactoring
of `pbf.rs`. **Effort**: Low-Medium — half a day. **Plan**: `docs/fable/ENH-002-pbf-test-fixture.md`

## ENH-003 — POI coverage completion (`man_made`/`natural`)

The Overpass query pays for `man_made` (tower/water_tower/chimney) and `natural`
(peak/rock/spring) nodes as always-included POIs, and the dedupe's `poi_category`
already treats `man_made` as a category — but the parsers classify only
`amenity|shop|tourism|leisure|historic`, so those fetched nodes land in the untagged
`nodes` map with their tags discarded (ARC-105 documented the drift; the remediation
deliberately preserved behavior). The enhancement is the product decision to actually
classify them: add the two keys to the (post-ARC-105) `POI_TAG_KEYS` constant, decide
value filtering (only the queried values, or any value), and version the change since
downstream POI counts will grow. Recovered data users already paid Overpass for.

**Expected impact**: recovers currently-discarded data; aligns query, classification,
and dedupe layers. **Effort**: Low — hours, mostly tests. **Requires maintainer
sign-off (output data changes).** **Plan**: `docs/fable/ENH-003-poi-coverage-completion.md`

## ENH-004 — Streaming Overpass fetch→parse pipeline

`fetch_osm_data` buffers the entire Overpass response into a `String` (hundreds of MB
for large areas — SEC-109 adds a cap but keeps full buffering), then hands it to the
parser. After QA-102 lands, the unified parse engine accepts any `BufRead` — and
`reqwest::blocking::Response` implements `Read` — so the response can stream directly
into the parser through a `BufReader`, never materializing the body. Peak memory for a
fetch drops from (body + parsed data) to roughly (parsed data) alone. Caching
complicates this: the raw body is currently written to the cache after parse, so the
stream must tee into the cache file (stream to a temp file while parsing, or parse from
the cached file after streaming to disk — the plan picks stream-to-disk-then-parse,
which is simpler and still eliminates the in-memory body).

**Expected impact**: ~50% peak-memory reduction on large fetches; removes the
single largest allocation in the crate. **Effort**: Medium — 1 day. **Depends on**:
QA-102 (unified engine). **Plan**: `docs/fable/ENH-004-streaming-overpass-parse.md`

## ENH-005 — Table-driven `map_tags_for_theme`

`map_tags_for_theme` (`src/overture/theme.rs:118`) is the #3 most complex function in
the crate (cyclomatic 26, "Critical" band per par-mem) — a hand-written match/if
cascade translating Overture properties to OSM tags per theme. Refactoring it to a
declarative mapping table (`&[(theme, source_key, target_tag, value_transform)]` or
per-theme `const` rule slices) collapses the branching into one interpreter loop,
makes each mapping independently visible/testable, and turns future Overture schema
additions into one-line table edits instead of new branches in a Critical-complexity
function.

**Expected impact**: complexity 26 → ~6; safer Overture schema evolution. **Effort**:
Low-Medium — half a day, behavior-pinned by existing tests. **Plan**:
`docs/fable/ENH-005-table-driven-theme-mapping.md`

## ENH-006 — Criterion benchmark regression tracking in CI

The three criterion benches (parse/write/merge) exist precisely to protect hot paths,
but they only run when a developer remembers to run them and eyeball the delta — the
remediation playbook's "baseline first, compare after" discipline is manual. Adding a
CI job that runs the benches on PRs and compares against the base branch (via
`critcmp` on criterion's saved baselines, or `cargo-codspeed`/`bencher` if a service
is acceptable) turns silent performance regressions into review comments. Runner noise
is the known hazard: the plan uses relative thresholds (~10%) and marks the job
non-required/informational.

**Expected impact**: performance regressions caught at PR time instead of by users.
**Effort**: Medium — a day of CI plumbing + threshold tuning. **CI change — maintainer
review per repo policy.** **Plan**: `docs/fable/ENH-006-ci-bench-regression-tracking.md`

## ENH-007 — 0.3.0 API modernization release plan

The audit deferred four breaking changes to a batched release: `BBox` newtype
replacing ~15 `(f64, f64, f64, f64)` signatures (ARC-106), an `OsmData` builder with
consistent field encapsulation (ARC-109), `OsmRelation::id` so round-trips stop
renumbering relations (ARC-113), and one unified progress-callback type (ARC-108) —
plus the SEC-105 cache-key contract already tightened in 0.2.x. Shipping them
piecemeal would break downstream (`osm-to-bedrock`, `osm-world`) repeatedly; shipping
them together with a migration guide breaks it once. This idea is the coordinated
release plan: ordering, deprecation shims where possible, a `MIGRATION-0.3.md`, and a
downstream compile-check pass before publish.

**Expected impact**: type-safe API (transposition bugs stop compiling), one downstream
migration instead of four. **Effort**: High — 2–3 days + downstream PRs. **Plan**:
`docs/fable/ENH-007-v0.3-api-modernization.md`

## ENH-008 — Tag-storage allocation reduction (key interning)

Every parsed element allocates owned `String`s for tag keys, yet OSM tag keys are
drawn from a tiny hot vocabulary (`highway`, `building`, `name`, `amenity`, …
repeated millions of times in a large extract). Switching tag maps from
`HashMap<String, String>` to `HashMap<Arc<str>, String>` with a parse-time interner
(pre-seeded with the ~50 most common keys, falling back to on-the-fly interning)
eliminates one allocation per tag for hot keys and shrinks resident memory for large
datasets. This changes the public type of every `tags` field, so it is 0.3.0-class —
sequence it inside or after ENH-007. Benchmarks must gate it: the win is real for
planet-scale extracts but must be proven non-regressive for small ones.

**Expected impact**: measurable allocation/memory reduction on large extracts (to be
quantified by the ENH-006 bench harness first). **Effort**: High — touches every
producer/consumer of tags. **Depends on**: ENH-007 (breaking batch), ideally ENH-006
(bench gate). **Plan**: `docs/fable/ENH-008-tag-key-interning.md`
