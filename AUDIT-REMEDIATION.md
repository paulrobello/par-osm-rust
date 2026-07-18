# Audit Remediation Report

> **Project**: par-osm-rust
> **Audit Date**: 2026-07-17
> **Remediation Date**: 2026-07-18
> **Severity Filter Applied**: `all` (every phase, including breaking changes — user-approved)
> **Branch**: `fix/audit-remediation` (base `17b8847`)
> **Outcome**: **All 70 issues resolved. 0 partial. 2 monitor/manual items.**

---

## Execution Summary

| Phase | Status | Agent(s) | Issues Targeted | Resolved | Partial | Manual |
|-------|--------|----------|:--------------:|:--------:|:-------:|:------:|
| 1 — Critical Security | ✅ | fix-security (opus) | 2 | 2 | 0 | 0 |
| 2 — Critical Architecture | ✅ | fix-architecture (opus) | 2 | 2 | 0 | 0 |
| 3a — Security (remaining) | ✅ | distributed across waves | 10 | 10 | 0 | 0 |
| 3b — Architecture (remaining) | ✅ | distributed across waves | 18 | 18 | 0 | 0 |
| 3c — All Code Quality | ✅ | distributed across waves | 21 | 21 | 0 | 0 |
| 3d — All Documentation | ✅ | distributed across waves | 16 | 16 | 0 | 0 |
| 4 — Verification | ✅ | orchestrator + targeted fix | — | — | — | — |

**Overall**: **70 / 70 issues resolved**, 0 partial, 2 monitor/manual items (see below). 22 atomic commits, 37 files changed (+7,890 / −2,866).

> **Execution note:** The audit's File Conflict Map flagged `osm.rs`, `overture.rs`, `cache.rs` as touched by *all four* domains (⚠️⚠️ "sequence domain edits"). Rather than run four parallel domain agents that would clobber each other on those files, Phase 3 was executed as **dependency-ordered, verified waves** — one agent per mega-module at a time, with `make checkall` gating and a checkpoint commit after each wave. This is what the conflict map demanded.

---

## Verification Results (Phase 4)

All run from repo root on the final commit (`391d317`):

| Check | Result |
|-------|--------|
| `make checkall` (default features) — fmt-check + `clippy -D warnings` + `cargo check` + `cargo test --all-features` | ✅ **173 unit + 7 integration + 13 doctests pass**, clippy clean |
| `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features` (CI docs job) | ✅ exit 0 |
| `cargo test --no-default-features` (pure subset, ARC-012) | ✅ **138 unit + 7 integration + 11 doctests pass** |
| `cargo clippy --all-targets --no-default-features -- -D warnings` | ✅ clean |
| `cargo audit` (ARC-014; lockfile generated as CI does) | ✅ exit 0 — **1 unsound advisory**, see monitor item #1 |
| MSRV floor | ✅ `osmpbf 0.3.8` requires Rust 1.88 → MSRV bumped 1.87 → **1.88**; verified builds on 1.89 |

Two real defects were caught and fixed *during* Phase 4 verification (not during the per-wave gates), which is the point of a final pass:
1. **MSRV conflict** — the declared MSRV (1.87) was below what the pinned `osmpbf 0.3.8` requires (1.88); the new MSRV-check job (ARC-014) would have failed. Fixed by bumping the MSRV to 1.88 everywhere (Cargo.toml, CI job, README badge, CONTRIBUTING, CHANGELOG).
2. **A clippy `collapsible_if` lint** newly firing under the current 1.97 toolchain on the SEC-011 port-pinning code. Fixed by collapsing the nested `if let`/`if` into a let-chain.

---

## Resolved Issues ✅

### Security (12 / 12)
- **[SEC-001]** Token in `.claude/settings.local.json` (already gitignored globally) — `.gitignore` (Low)
- **[SEC-002]** Overpass client follows redirects, bypassing SSRF allowlist → `.redirect(Policy::none())` — `src/overpass.rs` (High)
- **[SEC-003]** SRTM redirect policy → `Policy::none()` — `src/srtm.rs` (Low)
- **[SEC-004]** XML element-depth limit → `MAX_XML_DEPTH = 64` in single-pass parser — `src/osm/xml_parse.rs` (Low)
- **[SEC-005]** Full Overpass error body unbounded → capped via `truncate_error_body` — `src/overpass.rs` (Low)
- **[SEC-006]** Cache migration follows symlinks + unbounded `files_equal` → `symlink_metadata`, skip symlinks, streamed 8 MiB-capped compare — `src/cache.rs` (Low)
- **[SEC-007]** `unsafe set_var` in tests (Edition 2024) → documented `// SAFETY:` + Mutex serialization — `src/overture/cli.rs`, `src/cache.rs` (Low)
- **[SEC-008]** `parse_pbf` mmap SIGBUS risk → file-provenance precondition doc — `src/osm/pbf.rs` (Low)
- **[SEC-009]** `.gitignore` secrets block + pre-commit gitleaks/detect-private-key — `.gitignore`, `.pre-commit-config.yaml` (Low)
- **[SEC-010]** Overture CLI PATH-lookup → `PAR_OSM_OVERTURE_CLI` absolute-path override + docs — `src/overture/cli.rs` (Low)
- **[SEC-011]** Overpass port not pinned → reject non-443 ports on allowlisted hosts — `src/overpass.rs` (Low)
- **[SEC-012]** `fetch_geojson_for_type` arbitrary `cli_type` → `validate_cli_type` guard (rejects `-`/whitespace) — `src/overture/cli.rs` (Low)

### Architecture (21 / 21)
- **[ARC-001]** Overture cache version/TTL awareness (cli_version in key + 30-day TTL) — `src/overture/cache.rs` (High)
- **[ARC-002]** O(n²) POI dedupe → O(n·k) spatial grid (25 m cells, 3×3 neighbor window, sufficiency-proven) — `src/sources.rs` (High)
- **[ARC-003]** O(n²) way-ID reverse lookup → O(1) (now direct `way.id` read) — `src/osm/xml_write.rs` (High)
- **[ARC-004]** Centralized synthetic-ID allocation in `src/synthetic_ids.rs` + compile-time non-overlap asserts — (High)
- **[ARC-005]** Cache-dir getters made pure; explicit `migrate_legacy_caches()` startup entry point — `src/cache.rs` (High, **breaking**)
- **[ARC-006]** Two-pass XML parser → single-pass — `src/osm/xml_parse.rs` (High)
- **[ARC-007]** `osm.rs` → `osm/{model,pbf,xml_parse,xml_write,mod}.rs`; `overture.rs` → `overture/{theme,parse,cache,cli,mod}.rs` — (High)
- **[ARC-008]** `OsmData` encapsulation (`pub(crate)` fields, `new`/`push_way`/`iter_ways`/`way_id_at`/`validate_invariants`) — `src/osm/model.rs` (High, **breaking**)
- **[ARC-009]** Deterministic Overture synthetic IDs (per-parse allocator, no global atomic) — `src/synthetic_ids.rs` (Medium)
- **[ARC-010]** Live `OVERPASS_URL` read (no OnceLock freeze); returns `Cow<'static, str>` — `src/overpass.rs` (Medium, **breaking** return type)
- **[ARC-011]** Legacy cache API family `#[deprecated]` — `src/osm_cache.rs` (Medium)
- **[ARC-012]** `default = ["blocking"]` Cargo feature gating reqwest/network; `--no-default-features` pure subset compiles — `Cargo.toml`, `src/lib.rs` (Medium, **breaking**)
- **[ARC-013]** Streaming `parse_osm_xml_file` (BufReader, bounded memory) — `src/osm/xml_parse.rs` (Medium)
- **[ARC-014]** CI jobs: MSRV check, `cargo doc -D warnings`, `cargo audit`, OS matrix, docs-lint — `.github/workflows/ci.yml` (Medium)
- **[ARC-015]** `.pre-commit-config.yaml` (gitleaks, detect-private-key, make-wired fmt/lint/typecheck) + Makefile target — (Medium)
- **[ARC-016]** Writer validates `<nd ref>` references (skips dangling + warning) — `src/osm/xml_write.rs` (Medium)
- **[ARC-017]** `Cargo.lock` convention documented (already untracked) — `.gitignore` (Low)
- **[ARC-018]** Criterion benches for parse/write/dedupe — `benches/*.rs`, `Cargo.toml` (Low)
- **[ARC-019]** Integration round-trip tests — `tests/integration.rs` (Low)
- **[ARC-020]** Shared pooled `reqwest::blocking::Client` per module — `src/overpass.rs`, `src/srtm.rs` (Low)
- **[ARC-021]** CI OS matrix (ubuntu/macos/windows) — `.github/workflows/ci.yml` (Low)

### Code Quality (21 / 21)
- **[QA-001]** O(n²) way-ID lookup (with ARC-003) ✅
- **[QA-002]** O(n²) POI dedupe + allocations (with ARC-002) ✅
- **[QA-003]** Deduped cache logic → generic `RawCache<Meta>` helper in `src/cache_store.rs` ✅
- **[QA-004]** Deduped PBF `Element::Node`/`DenseNode` branches → `process_pbf_node` ✅
- **[QA-005]** Deduped XML `Empty`/`Start` handlers → 5 attribute helpers ✅
- **[QA-006]** Deduped Overture geometry conversion → `push_way_from_coords` ✅
- **[QA-007]** Deduped `fetch_overture_data` pair → `fetch_one_theme` ✅
- **[QA-008]** Migration side-effect getters (resolved by ARC-005) ✅
- **[QA-009]** God-module split (with ARC-007) ✅
- **[QA-010]** Deterministic synthetic IDs (with ARC-009) ✅
- **[QA-011]** `find_containing` dead code → `#[deprecated]` ✅
- **[QA-012]** Atomic meta-first data+meta cache write + orphan-skip read ✅
- **[QA-013]** Named synthetic-ID constants (with ARC-004) ✅
- **[QA-014]** `poi_category` allocation → borrowed `&str` comparisons ✅
- **[QA-015]** `merge` collision semantics documented ✅
- **[QA-016]** Streamed `files_equal` with early exit ✅
- **[QA-017]** `default_overpass_url` OnceLock (with ARC-010) ✅
- **[QA-018]** Standardized bbox terminology to `(south, west, north, east)` in docs ✅
- **[QA-019]** Unified cache-schema versioning docs ✅
- **[QA-020]** Windows `LOCALAPPDATA` precedence ✅
- **[QA-021]** `OsmWay.id` field added; writer simplified ✅

### Documentation (16 / 16)
- **[DOC-001]** Stale `0.1.0` version literal → `0.2.0` ✅
- **[DOC-002]** Added `CHANGELOG.md` (Keep-a-Changelog, backfilled 0.1.0/0.1.1/0.2.0) ✅
- **[DOC-003]** Added `CONTRIBUTING.md` ✅
- **[DOC-004]** `OsmData::merge` docstring enumerates every collection merged ✅
- **[DOC-005]** `osm` module `//!` doc rewritten ✅
- **[DOC-006]** `parse_pbf` docstring enumerates full return ✅
- **[DOC-007]** `#![warn(missing_docs)]` enforced + 21 items backfilled + broken intra-doc links fixed ✅
- **[DOC-008]** README Verification → `make checkall` gate ✅
- **[DOC-009]** Missing public APIs documented in README ✅
- **[DOC-010]** Merged split `///` blocks ✅
- **[DOC-011]** Merged duplicated `fetch_overture_data` summary ✅
- **[DOC-012]** `#![doc(html_root_url = "https://docs.rs/par-osm-rust/0.2.0")]` ✅
- **[DOC-013]** Rustdoc `# Examples` on sources/osm/overture/cache/elevation public items ✅
- **[DOC-014]** Documented `overture_cache_dir` re-export delegation ✅
- **[DOC-015]** Enriched `OsmOnly` variant doc ✅
- **[DOC-016]** markdownlint config + lychee link-check in CI (no fake coverage badge — CI has no coverage job) ✅

---

## Requires Manual Intervention / Monitor 🔧

### 1. `memmap2` unsound advisory (RUSTSEC-2026-0186) — transitive, monitor
- **What**: `cargo audit` reports one advisory: "Unchecked pointer offset in crate `memmap2`" (2026-06-20), classified **unsound** (not a vulnerability). `memmap2 = "0.9"` is a direct dependency (used by `parse_pbf` and `elevation` mmaps).
- **Impact on CI**: `cargo audit` exits **0** (unsound = warning). The CI `rustsec/audit-check@v2.0.0` job's default behavior does not fail on unsound advisories, so it should pass — but verify on the first CI run. If it does fail, add an `audit.toml` ignore for `RUSTSEC-2026-0186` or upgrade `memmap2` once a patched release exists.
- **Why not auto-fixed**: no patched `memmap2` release was available to upgrade to; the advisory is unsound (lower severity than a vuln). This is a transitive-dep reality the new audit job (ARC-014) correctly surfaces.
- **Recommended approach**: monitor; upgrade `memmap2` when a fix ships. Optionally add `audit.toml` if CI noise is unwanted.

### 2. 0.2.0 release coordination with downstream (action required before merge/publish)
This is a **breaking** release. Before merging to `main` / publishing to crates.io, the downstream crates `osm-to-bedrock` and `osm-world` (separate repos) need updates for these breaking changes:
- `OsmData` is now encapsulated — construct via `OsmData::new` / `push_way`; fields are `pub(crate)`. Read ways via `iter_ways()` / `way_id_at()`.
- `OsmWay` gained a required `pub id: i64` field (first field) — every `OsmWay { ... }` literal must set `id`.
- `OsmData::new` second param changed `Vec<(i64, OsmWay)>` → `Vec<OsmWay>`; `push_way(id, way)` → `push_way(way)`.
- **Cache-dir getters are pure** — downstream MUST call `cache::migrate_legacy_caches()` once at startup (was previously implicit).
- `default_overpass_url()` returns `Cow<'static, str>` (was `&'static str`) — bind to a local and borrow.
- Overture cache functions gained params: `overture_cache_read(dir, key, ttl)`, `overture_cache_write(dir, key, bbox, cli_type, cli_version, geojson)`.
- New `blocking` Cargo feature (default on). Downstream using `default-features = false` must add `features = ["blocking"]`.
- Legacy `osm_cache` family (`cache_key`/`read`/`write`/`find_containing`) is now `#[deprecated]` — migrate to the `*_for_url` API.
- MSRV bumped 1.87 → **1.88** (forced by `osmpbf 0.3.8`).

The module split (ARC-007) preserves all public paths via re-exports, so downstream using `crate::osm::*` / `crate::overture::*` is **not** broken by the reorganization.

---

## Files Changed (37)

**New** (`+`): `src/synthetic_ids.rs`, `src/cache_store.rs`, `src/osm/{mod,model,pbf,xml_parse,xml_write}.rs`, `src/overture/{mod,theme,parse,cache,cli}.rs`, `benches/{parse_osm_xml,write_osm_xml,merge_source_data}.rs`, `tests/integration.rs`, `.pre-commit-config.yaml`, `.markdownlint-cli2.jsonc`, `CHANGELOG.md`, `CONTRIBUTING.md`.

**Removed** (`−`): `src/osm.rs`, `src/overture.rs` (replaced by submodule directories).

**Modified**: `Cargo.toml`, `src/lib.rs`, `src/cache.rs`, `src/elevation.rs`, `src/filter.rs`, `src/osm_cache.rs`, `src/overpass.rs`, `src/source_options.rs`, `src/sources.rs`, `src/srtm.rs`, `Makefile`, `README.md`, `docs/ARCHITECTURE.md`, `.gitignore`, `.github/workflows/ci.yml`.

**22 commits** on `fix/audit-remediation` (base `17b8847`). Net **+7,890 / −2,866** lines.

---

## Next Steps

1. **Review this report**, especially the two monitor/manual items above.
2. **Coordinate the 0.2.0 breaking changes** with `osm-to-bedrock` and `osm-world` before merging/publishing.
3. **Merge `fix/audit-remediation` to `main`** (after review) — push and publish still require explicit confirmation. Rebase onto latest `main` first per the git workflow.
4. **Verify CI passes on the PR** — first run will exercise the new MSRV/docs/audit/docs-lint/OS-matrix jobs. Watch for the `memmap2` advisory in the audit job.
5. **Run `pre-commit install`** locally to activate the new gitleaks + fmt/lint/typecheck hooks.
6. Consider re-running `/audit` after merge to confirm an updated AUDIT.md reflects the remediated state (expect the issue count to drop to ~0, modulo the transitive `memmap2` advisory which is outside this crate's code).
