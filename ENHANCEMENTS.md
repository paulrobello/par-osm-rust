# Enhancement Ideas — par-osm-rust

> **Generated**: 2026-07-18 (Fable audit cycle, commit `55187f4`, v0.2.1)
> **Scope**: opportunities beyond the defect findings in `AUDIT.md` — performance,
> coverage, API evolution, and maintainability investments.
> **Per-idea implementation plans**: `docs/fable/ENH-XXX-<slug>.md` — each written to be
> executable by a smaller model without further analysis.
>
> **Maintenance rule**: this file tracks **open work only**. When an idea ships,
> **remove it from this list** — the permanent record of what changed and why lives in
> `CHANGELOG.md`, the per-idea plan doc under `docs/fable/`, and git history, so keeping
> it here duplicates those sources and obscures what is actually left to do. Mark an
> idea `[~]` if it is partially done (state the remaining work inline), and remove an
> idea outright only if it is rejected (note why).
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
| 1 | ENH-006 | Criterion benchmark regression tracking in CI | Medium (guardrail) | Medium | [ ] |
| 2 | ENH-008 | Tag-storage allocation reduction (key interning) | Medium (perf/memory) | High | [ ] |

---

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

## ENH-008 — Tag-storage allocation reduction (key interning)

Every parsed element allocates owned `String`s for tag keys, yet OSM tag keys are
drawn from a tiny hot vocabulary (`highway`, `building`, `name`, `amenity`, …
repeated millions of times in a large extract). Switching tag maps from
`HashMap<String, String>` to `HashMap<Arc<str>, String>` with a parse-time interner
(pre-seeded with the ~50 most common keys, falling back to on-the-fly interning)
eliminates one allocation per tag for hot keys and shrinks resident memory for large
datasets. This changes the public type of every `tags` field, so it is 0.5.0-class —
ENH-007 shipped in 0.3.0 (the crate is now at 0.4.0), so this sequences into the next
breaking release. Benchmarks must gate it: the win is real for planet-scale extracts
but must be proven non-regressive for small ones.

**Expected impact**: measurable allocation/memory reduction on large extracts (to be
quantified by the ENH-006 bench harness first). **Effort**: High — touches every
producer/consumer of tags. **Depends on**: ENH-007 (✓ shipped in 0.3.0), ideally
ENH-006 (bench gate). **Plan**: `docs/fable/ENH-008-tag-key-interning.md`
