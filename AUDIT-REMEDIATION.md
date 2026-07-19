# Audit Remediation Report

> **Project**: par-osm-rust
> **Audit Date**: 2026-07-18 (AUDIT.md @ commit `55187f4`, v0.2.1)
> **Remediation Date**: 2026-07-18
> **Severity Filter Applied**: `all` (every phase executed)
> **Branch**: `fix/audit-remediation` (base `55187f4` → HEAD `ff2b86b`, 7 commits)

---

## Execution Summary

| Phase | Status | Agent (model) | Issues Targeted | Resolved | Deferred | Manual |
|-------|--------|---------------|----------------:|---------:|---------:|-------:|
| 1 — Promoted Security | ✅ | fix-security (opus) | 5 | 5 | 0 | 0 |
| 2 — Critical Architecture | ✅ | fix-architecture (opus) | 4 | 4 | 0 | 0 |
| 3a — Security (remaining) | ✅ | fix-security (sonnet) | 4 | 4 | 0 | 0 |
| 3b — Architecture (remaining) | ✅ | fix-architecture (opus) | 12 | 5 | 7 | 0 |
| 3c — Code Quality | ✅ | fix-code-quality (opus) | 15 | 15¹ | 0 | 0 |
| 3d — Documentation | ✅ | fix-documentation (sonnet) | 11 | 11 | 0 | 0 |
| 4 — Verification | ✅ | orchestrator | — | — | — | — |

¹ QA-111, QA-112, and QA-115 were resolved as duplicates of ARC-112, ARC-111, and ARC-111 respectively during Phase 3b (one implementation each, per the audit's cross-domain duplicate map).

**Overall**: **44 issues resolved**, **7 deferred** to the planned 0.3.0 breaking batch / pending security review, **0 requiring manual intervention**. All three maintainer decisions were captured up front (ARC-102 → deprecate; all opt-in security/CI items → approved; DOC-003 → commit-link fallback).

---

## Resolved Issues ✅

### Security (13)

- **[SEC-101]** Bound SRTM gzip decompression + response buffering — `src/srtm.rs`. `decode_hgt_gz` extracted (network-free testable); `GzDecoder::new(...).take(MAX_HGT_BYTES + 1)` bounds the decompressed read; `Content-Length` > 30 MiB pre-check; decompressed size validated against the two legal HGT sizes (25,934,402 / 2,884,802). 4 new tests.
- **[SEC-102]** `tiles_for_bbox` validates finite/±90/±180/min<max and caps at `MAX_SRTM_TILES=1000`; now returns `anyhow::Result`. 6 new tests.
- **[SEC-104]** Shared `pub(crate) validate_bbox` helper (new `src/bbox.rs`) used by `tiles_for_bbox` and `build_overpass_query`; NaN/inf/out-of-range now rejected. 9 new tests.
- **[SEC-105]** `RawCache::validate_key` rejects empty / non-`[0-9a-zA-Z_-]` keys at the boundary; alphabet constraint documented on all public key-taking APIs. 5 new tests.
- **[SEC-103]** `parse_pbf` rustdoc corrected (buffered I/O, not mmap); `cargo audit --deny unsound` gate added; `.cargo/audit.toml` ignores verified-unreachable RUSTSEC-2026-0186. *(The playbook said repo-root `audit.toml`; the agent empirically verified cargo-audit 0.22.2 only honors `.cargo/audit.toml` and corrected it — verified locally, `cargo audit --deny unsound` → exit 0.)*
- **[SEC-106]** All third-party Actions pinned to commit SHAs (dereferenced through annotated tags; each verified to resolve); top-level `permissions: contents: read` added to ci.yml; unmaintained `Ilshidur/action-discord` replaced with a first-party `curl` step. `actionlint` → exit 0.
- **[SEC-107]** `Cargo.lock` committed (removed from `.gitignore`); `cargo generate-lockfile` dropped from CI audit job; policy reversal documented in CHANGELOG. *(Reverses documented ARC-017 — maintainer-approved.)*
- **[SEC-108]** SRTM tile writes via `tempfile::NamedTempFile::new_in` + `persist` (defeats symlink pre-planting and the concurrent-writer race; preserves mmap atomicity).
- **[SEC-109]** Overpass response bounded via `Read::take` (2 GiB success path, 64 KiB error-body path).

### Architecture (10)

- **[ARC-101]** 🔴 **Critical fix** — one `OvertureIdAllocator` threaded through each fetch via new `pub(crate) parse_overture_geojson_with_allocator`; eliminates silent geometry corruption from cross-parse ID collisions. Regression + rationale tests added.
- **[ARC-103]** `OsmData::merge` gains the documented `debug_assert!(validate_invariants)` (mirroring `clip_to_bbox`) as its last statement, plus skip-first-wins collision defense with `log::warn`.
- **[ARC-102]** `ThemePriority` / `priority_for` / `OvertureParams::priority` + the three `source_options` parsers marked `#[deprecated(since="0.2.2")]`; README + `PoiSourceMode` rustdoc corrected; CHANGELOG noted. *(Maintainer decision: deprecate.)*
- **[QA-102 ≡ ARC-104]** The two ~200-line XML parser event loops unified into one generic `parse_osm_events<R: BufRead>`. `xml_parse.rs` net-shrank ~178 lines. Parse bench neutral-to-faster (≤5.9% improvement).
- **[ARC-105]** Single `pub(crate) POI_TAG_KEYS` constant in `osm::model`, consumed by the XML/PBF parsers and `poi_category`; `man_made` kept as a dedupe-only extra (no classification change); the Overpass query's intentional over-fetch now documented. Zero behavior change.
- **[ARC-110]** `platform_cache_root` uses `cfg!(windows)` gating; honors `XDG_CACHE_HOME` on unix; macOS kept on `~/.cache` to avoid orphaning existing caches. New unix tests.
- **[ARC-111 ≈ QA-112]** Overture CLI fetch loops unified into `fetch_overture_with_policy` (`FailurePolicy::FailFast|BestEffort`); three spawn/poll/timeout sites share `wait_with_timeout`. Public error text preserved. QA-115 pipe-buffer comment folded in.
- **[ARC-112 ≡ QA-111]** `osm_cache` listing carries `CacheMeta` via `list_metas_in`; `find_containing_in_for_url` filters metas directly (zero per-candidate re-reads).
- **[ARC-109 interim]** `OsmData` struct doc corrected to honestly describe `pub` vs `pub(crate)` field visibility (full builder deferred to 0.3.0).

### Code Quality (15)

- **[QA-101]** Duplicate/missing way IDs handled at the parse boundary (skip + `log::warn`, first-wins) in both the unified XML engine and the PBF parser; `OsmData::new`'s debug assert becomes a true internal invariant. 6 new tests.
- **[QA-103]** PBF per-way `tags`/`node_refs` clones → moves.
- **[QA-104]** `mem::take` in the unified XML engine (way/relation/node arms; node dual-consumer handled). Parse bench neutral (within band).
- **[QA-105]** `merge_source_data` POI clones reduced arm-by-arm (mandatory data-flow read per arm). `merge_source_data` bench −2.5% to −3.7%.
- **[QA-106]** `clip_to_bbox` filters ways by move (`mem::take` releases the borrow). Per-`fetch_map_data` way clones eliminated.
- **[QA-107]** Shared crate-private `text_truncate` module (overpass + overture::cli), `blocking`-gated. 7 new tests.
- **[QA-108]** `cache_store` atomic writes via `tempfile::NamedTempFile` + `persist` (srtm half done in SEC-108); meta-first/data-last ordering preserved.
- **[QA-109]** Cache-migration overture exception documented from git history (commit `d6b224c`).
- **[QA-110]** Discarded Base-theme `OsmPoiNode` clone removed (construction moved into Address/Place arms).
- **[QA-111 ≡ ARC-112]** Single metadata read — resolved in Phase 3b.
- **[QA-112 ≈ ARC-111]** CLI spawn/orchestration dedup — resolved in Phase 3b.
- **[QA-113]** Shared `to_hex` helper; 4 per-byte `format!` sites replaced. 3 new tests.
- **[QA-114]** Single-pass XML attribute escaping (byte-identical, round-trip pinned). `write_osm_xml` bench **−17% to −19%**.
- **[QA-115]** Pipe-buffer comment — folded into ARC-111 in Phase 3b.
- **[QA-116]** Zero-TTL write semantics documented (behavior unchanged).

### Documentation (11)

- **[DOC-002]** Kept the `# Errors`/`# Panics` convention; **21 new `# Errors` sections** added across 11 files (each derived from the fn's actual error paths), 0 `# Panics` (none in library code). Crate-wide total now 34.
- **[DOC-001]** CONTRIBUTING.md 0.2.x sync — feature model, `missing_docs` enforcement, real docs CI command, real test layout, OvertureTheme split-file walkthrough.
- **[DOC-003]** CHANGELOG compare links repointed at release commits (SHA-based, all 5 verified HTTP 200); no tags created/pushed. README release checklist notes the tag TODO.
- **[DOC-004]/[DOC-005]** README deps → `"0.2"`; ARCHITECTURE Data Model documents the `nodes()`/`ways()`/`ways_by_id()` accessors.
- **[DOC-006]/[DOC-007]** README literal-bracket links → code spans; bare-`cargo test` claim reworded.
- **[DOC-008]** `CONTRIBUTING.md` + `CHANGELOG.md` added to markdownlint globs and lychee args.
- **[DOC-009]** README TOC (10 H2 bullets) + Documentation section links; ARC-102 priority text left intact.
- **[DOC-010]** CHANGELOG 0.2.0 `html_root_url` claim annotated (not rewritten).
- **[DOC-011]** `ReadmeDoctests` struct (`#[cfg(all(doctest, feature="blocking"))]`) compiles README examples under `cargo test --doc --all-features` (doctest count 13 → 18); `[package.metadata.docs.rs] all-features = true` added.

---

## Regressions Caught During Self-Verification 🔍

Two regressions were **introduced by the remediation itself** and **not caught by `make checkall`** — both surfaced because the orchestrator ran the full gate (including checks the Makefile omits) after each phase rather than trusting sub-agent self-reports. Both fixed immediately.

1. **7 rustdoc `private-intra-doc-link` errors** (caught after Phase 3a). Phases 1 & 2 added doc-comment links to private items (`parse_osm_events`, `validate_bbox`, `OvertureIdAllocator`, `MAX_HGT_BYTES`, …). `make checkall`'s recipe is `fmt-check lint typecheck test` — it does **not** run `cargo doc`, but the CI docs job (`RUSTDOCFLAGS=-D warnings cargo doc --all-features`, green at base) would have failed. Fixed by converting `[`private`]` links to backtick code spans. *(Commit `2a0b213`.)*

2. **`bbox::validate_bbox` dead-code error under `--no-default-features`** (caught after Phase 3c). The Phase 1 SEC-104 agent registered `bbox` as an un-gated `pub(crate) mod`, but its only callers (`overpass`, `srtm`) are `#[cfg(feature = "blocking")]`. `make checkall`'s lint runs `--all-features`, so it missed this; the crate's documented "pure subset compiles clean" property was broken. Fixed by gating `bbox` behind `blocking` — mirroring the `text_truncate` gating the QA-107 agent had correctly applied one phase earlier. *(Commit `44192a2`.)* `cargo clippy --no-default-features -- -D warnings` now green.

---

## Deferred (Planned, Not Manual Intervention) 🗓️

These are deliberately scheduled for a future release — not failures of this run.

### 0.3.0 breaking batch (one planned release; do not implement piecemeal)
- **[ARC-106]** `BBox` newtype — touches ~15 public signatures.
- **[ARC-108]** Unified progress-callback contract.
- **[ARC-109]** (full) `OsmData` builder / consistent encapsulation. *(Interim non-breaking doc fix applied this cycle.)*
- **[ARC-113]** `OsmRelation::id`.
- **SEC-105 contract tightening** (public key-alphabet newtype) — batch with the above.

### Pending security-domain review
- **[ARC-107]** Configurable Overpass host policy — additive opt-in, but expands the SSRF surface; needs explicit security approval before implementing (would extend SEC-002/SEC-011).

### Already addressed (no separate action)
- **[ARC-114]** SRTM — gzip half fixed by SEC-101; sequential-download parallelization is enhancement work (ENH-001).
- **[ARC-115]** Cargo.lock — resolved by SEC-107.

---

## Verification Results

| Check | Result |
|-------|--------|
| `make checkall` (fmt-check + clippy `--all-features -D warnings` + typecheck + test) | ✅ Pass |
| `cargo test --all-features` | ✅ 219 unit + 7 integration + 18 doc |
| `cargo test --no-default-features` (pure subset) | ✅ 158 unit + 7 integration + 11 doc |
| `cargo test --doc --all-features` | ✅ 18 (was 13; README guard +5) |
| `cargo clippy --no-default-features -- -D warnings` | ✅ Pass *(gates the gap that caught the bbox regression)* |
| `cargo doc --no-deps --all-features` (`RUSTDOCFLAGS=-D warnings`) | ✅ Pass |
| markdownlint (CI set: README + CONTRIBUTING + CHANGELOG + docs/) | ✅ 0 issues |
| `cargo audit --deny unsound` (with `.cargo/audit.toml`) | ✅ Pass (RUSTSEC-2026-0186 ignore in effect) |
| `actionlint .github/workflows/*.yml` | ✅ Exit 0 |
| Protected criterion benches (`parse_osm_xml`, `write_osm_xml`, `merge_source_data`) | ✅ All completed; no correctness-guard failures |

**Bench deltas** (vs pre-remediation baselines; all within the ±5% band or improved):

| Bench | Delta |
|-------|-------|
| `write_osm_xml` (QA-114) | **−17% to −19%** |
| `merge_source_data_dedupe` (QA-105) | −2.5% to −3.7% |
| `parse_osm_xml_{str,file}` (QA-102/QA-104) | neutral (≤±2.1%, within band) |

No regressions. `lychee` was not installed locally for the DOC-003/008 link check; all CHANGELOG compare URLs were verified HTTP 200 via curl instead.

---

## Files Changed

**33 files** (base `55187f4` → HEAD `ff2b86b`); 7 commits; +4,819 / −683 (of which `Cargo.lock` is +2,413 as a one-time tracked-file addition).

New files: `src/bbox.rs`, `src/text_truncate.rs`, `.cargo/audit.toml`, `Cargo.lock`.
Modified source: every `src/` module touched by the issues above, plus `Cargo.toml` (`[package.metadata.docs.rs]`).
Modified docs/config: `README.md`, `CONTRIBUTING.md`, `CHANGELOG.md`, `docs/ARCHITECTURE.md`, `.github/workflows/ci.yml`, `.github/workflows/publish-crates.yml`, `.gitignore`, `.markdownlint-cli2.jsonc`.

Commit graph:
```
ff2b86b docs: # Errors convention, sync docs to 0.2.1, README doctest guard (DOC-001..011)
44192a2 perf/correctness: hot-path clone removal, parse-boundary way-ID handling (QA-101..116)
a7a66fe refactor(architecture): consolidate POI keys, dedup Overture CLI (ARC-105/110/111/112)
0928e8f chore(security): pin CI actions, cargo-audit gate, commit Cargo.lock (SEC-103/106/107)
2a0b213 fix(security): bound Overpass buffering, pbf mmap doc (SEC-109, SEC-103) + doc-link fixes
342e533 fix(architecture): multi-theme Overture ID collision + unify XML parser (ARC-101/103/102, QA-102)
f90c441 fix(security): bound SRTM decompression/bbox, validate cache keys (SEC-101/102/104/105/108)
```

---

## Next Steps

1. **Review the user-approved opt-in changes** before merge — they were applied per your explicit approval but are flagged for manual review per audit policy: SEC-106 (supply-chain SHA pins + Discord→curl), SEC-107 (Cargo.lock policy reversal), SEC-103 (cargo-audit `--deny unsound` gate). Commits `0928e8f` and `2a0b213`.
2. **Merge `fix/audit-remediation` to `main`** (rebase onto latest `main` first if it has moved). Push and publish still require your explicit confirmation.
3. **Release tags (DOC-003 TODO)**: no tags were created (you chose the commit-link fallback). When convenient, creating annotated tags `v0.1.0`…`v0.2.1` at the release commits and repointing the CHANGELOG links is the durable fix — add the tag step to the README release checklist (already noted there).
4. **Close the pure-subset CI gap (recommendation, not in audit scope)**: this run discovered that `make checkall` and CI only lint/test under `--all-features`, so a `--no-default-features` regression (the bbox dead-code issue) was invisible until the orchestrator ran `cargo clippy --no-default-features -- -D warnings` manually. Adding a `cargo check/clippy --no-default-features` step to CI would prevent the class. *(This is a CI change — opt-in; raising it for your decision.)*
5. **Schedule the 0.3.0 batch**: ARC-106 / ARC-108 / ARC-109-full / ARC-113 (+ SEC-105 contract newtype) as one planned breaking release, with downstream (`osm-to-bedrock`, `osm-world`) migration sequenced after.
6. **ARC-107 security review**: decide whether to approve the configurable-Overpass-host opt-in.
7. **Re-run `/audit`** after merge to confirm the findings are closed and surface anything new.
8. **Decide on the audit artifacts**: `AUDIT.md`, `AUDIT-REMEDIATION-PLAN.md`, `ENHANCEMENTS.md`, and `docs/fable/ENH-*.md` are currently untracked. Keep, commit, or delete per your preference (the prior 0.2.0 cycle deleted its audit artifacts post-remediation).
