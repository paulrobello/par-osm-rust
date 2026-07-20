# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Released versions are published to [crates.io](https://crates.io/crates/par-osm-rust).

## [Unreleased]

### Added

- **Streaming cache primitives on `RawCache` (ENH-004).** The generic
  `cache_store::RawCache` gains three public methods: `data_path(&Key)` (the
  absolute `<dir>/<key>.<ext>` path, for handing a path to a streaming parser
  instead of reading a cached payload into a `String`); `stream_to_temp(reader,
  max_bytes)` (bounded copy into a cache-dir `NamedTempFile`, rejecting an
  oversized body mid-copy and leaving no orphan on rejection); and
  `commit_temp(key, meta, data_tmp)` (QA-012 meta-first / data-last commit that
  hands the surviving temp file back on a commit failure). All additive; no
  existing signature changed.

### Changed

- **`fetch_osm_data` now streams the Overpass response (ENH-004).** The
  response body is no longer buffered into an in-memory `String` before
  parsing. It is streamed straight into a cache-directory temp file bounded by
  the SEC-109 cap, then parsed from that file with the streaming XML parser;
  cache hits likewise parse the cached file by path. Peak memory on a large
  fetch drops from roughly (body + parsed data) to (parsed data) alone — about
  a 50% cut, eliminating the crate's single largest allocation. The prior
  non-fatal-cache-write contract is preserved: if the bounded copy succeeds but
  the cache commit fails, the fetch still parses from the surviving temp file
  and only warns. `fetch_osm_xml` is unchanged in behavior (now built over the
  shared `fetch_osm_response` request builder). Public API is unchanged.

## [0.4.0] - 2026-07-19

### Added

- **Parallel SRTM tile downloads (ENH-001).** `srtm::download_tiles_for_bbox`
  now fetches tiles concurrently through a bounded `std::thread::scope`
  worker pool (default 4 workers, set by the `SRTM_DOWNLOAD_CONCURRENCY`
  constant) instead of strictly sequentially, cutting multi-tile
  elevation-fetch wall-clock time by ~3–5× on network-bound workloads. All
  existing semantics are preserved: per-tile retry/backoff, error
  aggregation, and the progress-callback contract (progress events are
  forwarded to the caller thread via a channel, so the callback is never
  invoked from a worker thread). The public API is unchanged; no async
  runtime is introduced.

- **PBF format test coverage (ENH-002).** `parse_pbf` — the `.osm.pbf` input
  path — previously had zero test coverage (no `.pbf` fixture existed in the
  repo). Added a checked-in `tests/fixtures/pbf_parity.{osm,osm.pbf}` fixture
  pair (the PBF twin generated from the XML source via osmium-tool) plus three
  integration tests in `tests/integration.rs`: an XML↔PBF parity test across
  every `OsmData` collection (nodes, ways, POI/address/tree/tagged nodes,
  relations, bounds), a PBF-side classification pin, and a truncated-input
  error-path test. The parity test also exercises the `parse_osm_file`
  dispatcher's `.osm.pbf` → `parse_pbf` branch. Library behavior is unchanged;
  this closes the crate's largest test blind spot so future PBF-path changes
  (classification, relation ids, the duplicate-id policy) ship covered. Tests
  run under both `--all-features` and `--no-default-features`.

### Changed

- **`man_made` (tower/water_tower/chimney) and `natural` (peak/rock/spring)
  standalone nodes are now classified as POIs (ENH-003).** The XML and PBF
  parsers route these nodes into `OsmData::poi_nodes` with their tags retained,
  instead of dropping them into the plain `nodes` map. This aligns the
  classification layer with the Overpass query, which already fetches these
  nodes as always-included POIs. **`poi_nodes` counts grow for datasets that
  contain such nodes** — downstream consumers that assert exact POI counts
  should update them. `natural=tree` is unchanged (still routed to
  `tree_nodes`); other `man_made`/`natural` values (e.g. `man_made=pier`,
  `natural=water`) remain non-POIs. Classification is now driven by a single
  value-aware `osm::model::POI_TAG_RULES` table shared by both parsers and the
  dedupe helper `sources::poi_category` (the flat `POI_TAG_KEYS` constant is
  retired).

This release bundles three enhancements shipped since 0.3.1. **ENH-001**
(parallel SRTM tile downloads) is a wall-clock performance improvement with the
public API unchanged; **ENH-002** (PBF test coverage) adds test infrastructure
with no behavior change; **ENH-003** is the sole behavior change — standalone
`man_made`/`natural` POI nodes now land in `poi_nodes` instead of the plain
`nodes` map, so `poi_nodes` counts grow where such nodes are present. No API
breaks across any of the three; consumers on 0.3.x can bump with no code
changes.

## [0.3.1] - 2026-07-19

### Added

- **`OsmData::tagged_nodes` — lossless collection of every standalone tagged
  node (ARC-004).** The XML and PBF parsers now populate
  `tagged_nodes: Vec<OsmPoiNode>` with every standalone node carrying one or
  more tags, each retaining its full tag map. It is the lossless superset of
  the curated `poi_nodes` / `addr_nodes` / `tree_nodes` collections, which only
  classify `amenity`/`shop`/`tourism`/`leisure`/`historic`, `addr:housenumber`,
  and `natural=tree`. Consumers that classify on other tag keys (e.g.
  `natural=peak`, `man_made=*`) should read `OsmData::tagged_nodes()` — the
  curated collections silently drop such nodes. Read via the `tagged_nodes()`
  accessor, set via the `with_tagged_nodes` builder, and carried through
  `merge` and `clip_to_bbox`.

- **`write_osm_xml_string` round-trips every tagged node when `tagged_nodes`
  is populated.** A write → re-read cycle no longer discards nodes outside the
  curated classifications (`natural=peak`, `man_made=tower`, …) — previously
  those tags were silently lost on round-trip. When `tagged_nodes` is empty
  (an `OsmData` built directly via `with_poi_nodes` / `with_addr_nodes` /
  `with_tree_nodes` and never parsed), the writer keeps the pre-0.3.1 curated
  emission byte-for-byte, so that path is unchanged.

This release is additive only. Consumers on 0.3.0 can bump with no code
changes: `tagged_nodes` defaults to empty and the writer fallback preserves the
legacy builder-only behavior.

## [0.3.0] - 2026-07-18

The 0.3.0 release consolidates six interrelated breaking changes from the
audit. Every public signature change has a mechanical migration; the
individual entries below name the migration path. Consumers updating from
0.2.x should expect to touch every site that constructs an `OsmData`, a
bounding box, a cache key, or calls into `overpass::fetch_osm_xml` /
`fetch_osm_data` / `sources::fetch_map_data`.

### Breaking

- **`BBox` newtype for every bbox-taking public signature (ARC-106).**
  A new `pub struct BBox { south, west, north, east }` (with `Copy`,
  `PartialEq`, and serde derives) replaces the `(f64, f64, f64, f64)` tuples
  threaded through `srtm::tiles_for_bbox`/`download_tiles_for_bbox`,
  `overpass::build_overpass_query`/`fetch_osm_xml`/`fetch_osm_data`,
  `sources::fetch_map_data(_with_fetchers)` (plus the `FetchOsm`/`FetchOverture`
  generic bounds), every `osm_cache` cache-key and write/find function, and
  the `overture::cache` / `overture::cli` entry points. The validating
  constructor `BBox::new(s,w,n,e) -> Result<BBox>` runs the SEC-104 checks
  (non-finite / out-of-range / inverted-bound); `BBox::from_unchecked` and
  the blanket `From<(f64,f64,f64,f64)>` provide mechanical migration for
  already-validated input. `BBox::wsen()` adapts to the Overture CLI's WSEN
  ordering. The on-disk `[f64; 4]` cache-meta wire format is unchanged — `BBox`
  converts at the boundary so existing cache entries remain readable. The
  `OsmData::bounds` / `with_bounds` / `clip_to_bbox` signatures stay on tuples
  (not in ARC-106's migration list).

- **`OsmData` full encapsulation + `Default`/builder (ARC-109).** The five
  remaining public fields (`relations`, `bounds`, `poi_nodes`, `addr_nodes`,
  `tree_nodes`) are now `pub(crate)`, matching the encapsulation already in
  place for `nodes`/`ways`/`ways_by_id`. Five new read accessors mirror the
  existing pattern. `impl Default for OsmData` produces an empty starting
  point; the `with_nodes`/`with_ways`/`with_relations`/`with_bounds`/
  `with_poi_nodes`/`with_addr_nodes`/`with_tree_nodes` consume-self builder
  methods compose naturally. `OsmData::new(...)` is
  `#[deprecated(since="0.3.0")]`; the crate's own production code migrates to
  the builder, internal `#[cfg(test)]` modules add `#[allow(deprecated)]` for
  legacy coverage, and external callers (`tests/integration.rs`, benches,
  README doctests) migrate to the builder.

- **`OsmRelation::id` populated by parsers, emitted by writer (ARC-113).**
  `OsmRelation` gains `pub id: i64`, mirroring `OsmWay::id`. Both parsers
  populate it from `<relation id="…">` (XML) or the PBF `Relation::id` field,
  with skip-and-warn for missing/unparseable ids and first-wins on duplicates
  (mirroring QA-101's way-id handling). The XML writer emits the real id when
  present, falling back to the synthetic `writer_relation_id(idx)` only for
  id-less synthetic relations. A new round-trip test pins the parse→write→parse
  preservation of relation ids.

- **Unified `ProgressFn` contract across fetch APIs (ARC-108).** A new
  `pub type ProgressFn<'a> = &'a mut dyn FnMut(f32, &str)` at the crate root
  replaces the per-call-site `&mut dyn FnMut(f32, &str)` spellings.
  `srtm::download_tiles_for_bbox` migrates from the raw
  `&dyn Fn(usize, usize, &str)` counts callback to `ProgressFn`; the
  `(i, total)` pair is mapped to a fraction and the tile name flows into a
  status message of the form `"SRTM tile {name} ({i+1}/{total})"`. The
  clamping + monotonic-guard `emit_progress` helper moves to the crate root
  as `pub(crate)` and is used uniformly.

- **Cache `Key` newtype enforces SEC-105 alphabet at the type (0.3.0).**
  `pub struct Key(String)` in `cache_store` carries the validated-alphabet
  contract (`[0-9a-zA-Z_-]`, non-empty) as a type, not a runtime check
  repeated per call. `RawCache::{read_data, read_meta, write}`,
  `osm_cache::{read, read_for_url, write, write_for_url}`, and
  `overture::cache::{overture_cache_read, overture_cache_write}` take `&Key`.
  The `cache_key*` and `overture_cache_key*` helpers return `Key` (was
  `String`) and wrap their SHA-256 output via the pub(crate)
  `Key::from_sha256_hex` constructor with no redundant re-validation.
  `Key::new(s: &str) -> Result<Key>` is the public validating constructor
  for callers that hand-build keys. List-path defense-in-depth: an entry
  loaded from disk whose key fails re-validation is treated as a cache miss.

- **Configurable Overpass host allowlist (ARC-107).**
  `pub fn validate_overpass_url_with_hosts(url: &str, extra_hosts: &[String])`
  extends the SSRF host allowlist with consumer-supplied hosts (exact match
  only). **The relaxation is host-only** — HTTPS, no-userinfo, and port-443
  are enforced unconditionally even for an extra host. `validate_overpass_url`
  is retained as a thin delegating wrapper that passes `&[]`.
  `overpass::fetch_osm_xml`/`fetch_osm_data` take a trailing `&[String]`;
  `sources::SourceOptions` gains `pub extra_allowed_hosts: Vec<String>`
  (default empty); `sources::fetch_map_data_with_fetchers` threads
  `&options.extra_allowed_hosts` into the `FetchOsm` callback.

### Changed

- **Cargo.lock is now committed (SEC-107, reverses ARC-017).** Library crates
  can legitimately go either way per current Cargo guidance; this repo now
  commits the lockfile so CI, `cargo audit`, and publishes resolve a fixed
  dependency graph (249 transitive crates), giving a meaningful review point
  for any version drift. The audit job no longer regenerates the lockfile.
  Maintenance: run `cargo update` periodically (roughly monthly) and review
  the diff before merging — a Dependabot `cargo` entry may replace this
  manual cadence in a future release.

- **Overpass response buffering is now bounded (SEC-109).** `fetch_osm_xml`
  reads successful responses through a `take(MAX + 1)` adapter capped at
  2 GiB — generous, since large-area queries legitimately return hundreds of
  MB — and rejects oversized responses without buffering them fully. The
  error-path read is bounded at 64 KiB (only 4 KiB is surfaced into the
  error message after truncation, so reading more was waste).

- **Platform-correct cache root resolution (ARC-110).** The shared cache
  root is now resolved per platform conventions: on Windows `LOCALAPPDATA`
  is preferred (unchanged) and `HOME/.cache/<app>` is the fallback; on Unix
  (incl. macOS) `XDG_CACHE_HOME` is honored when set and non-empty, then
  `HOME/.cache/<app>` is used as before. macOS deliberately stays on
  `~/.cache/<app>` rather than `~/Library/Caches/<app>` so existing user
  caches written by 0.1.x/0.2.x are not orphaned — migration to the
  macOS-conventional location is deferred to a future release. The
  `cfg!(windows)` gate is compile-time, so a misconfigured unix shell that
  exports `LOCALAPPDATA` can no longer override the XDG/HOME-based path.
  Explicit per-cache env overrides (`PAR_OSM_*_CACHE_DIR` → `*_CACHE_DIR` →
  shared default) still win over all platform logic.

- **Internal dedup without behavior change (ARC-105, ARC-111, ARC-112).**
  POI classification tag keys (`amenity`/`shop`/`tourism`/`leisure`/
  `historic`) are now expressed once via a single `POI_TAG_KEYS` constant
  in `osm::model` consumed by both parsers and the dedupe helper
  (ARC-105); the duplicated fetch/spawn/poll loops in the Overture CLI
  orchestrator collapse into one policy-parameterized implementation plus
  a shared `wait_with_timeout` subprocess helper (ARC-111); and the
  Overpass cache containment lookup no longer re-reads each candidate's
  metadata file (ARC-112). None of these change observable behavior —
  parse/PBF/dedupe/cache test suites pass untouched.

### Security

- **CI supply-chain hardening (SEC-106).** All third-party GitHub Actions
  across `ci.yml` and `publish-crates.yml` are pinned to full commit SHAs
  (with the prior tag preserved as a comment), `ci.yml` now declares a
  top-level `permissions: contents: read`, and the unmaintained
  `Ilshidur/action-discord@0.4.0` publish-notification step (which received
  the `DISCORD_WEBHOOK` secret) is replaced with a first-party `curl` call.
  Pinned actions: `actions/checkout@de0fac2e` (v6.0.2),
  `dtolnay/rust-toolchain@4cda84d5` (stable branch),
  `Swatinem/rust-cache@c1937114` (v2.9.1),
  `taiki-e/install-action@07b4745e` (v2.83.4),
  `DavidAnson/markdownlint-cli2-action@8de2aa07` (v24.0.0),
  `lycheeverse/lychee-action@e7477775` (v2.9.0).

- **`cargo audit --deny unsound` CI gate (SEC-103).** The audit job now
  fails the build on any new "unsound" advisory rather than exiting 0.
  `.cargo/audit.toml` ignores `RUSTSEC-2026-0186` (`memmap2 0.5.10` via
  `osmpbf 0.3.8`), which is verified-unreachable: `parse_pbf` reads via
  `osmpbf`'s `ElementReader` → `BlobReader` → `BufReader` (buffered I/O),
  not `mmap_blob`. The ignore will be removed when osmpbf bumps memmap2 to
  ≥ 0.9.

- **`parse_pbf` doc correction (SEC-103).** The rustdoc previously claimed
  `parse_pbf` memory-mapped its input and warned about a `SIGBUS` hazard if
  the backing file shrank underneath the reader. This was incorrect —
  `ElementReader::from_path` streams blobs through `BufReader<File>` and
  never calls `mmap_blob`. The doc now accurately describes the buffered
  I/O model and notes the `memmap2` advisory is unreachable from this path.

### Deprecated

- **`ThemePriority` API surface (ARC-102).** `overture::ThemePriority`,
  `OvertureParams::priority`, `OvertureParams::priority_for`, and the
  `source_options` parsers `parse_theme_priority`, `parse_overture_priority`,
  and `parse_overture_priority_map` are all marked
  `#[deprecated(since = "0.2.2", note = "never implemented; will be removed in 0.3.0")]`.
  The promised theme-priority filtering (`priority = { Building: Osm }` to
  exclude Overture buildings) was never implemented — every merge
  unconditionally keeps both sources' non-POI geometry. The deprecated
  surface stays through 0.3.0 to preserve config-file compatibility; the
  `priority` map is still parsed and serialized, it is just never consulted.
  README and `PoiSourceMode::OsmOnly` rustdoc corrected to drop the claim.

## [0.2.1] - 2026-07-18

### Added

- **`OsmData` read accessors.** `nodes()`, `ways()`, and `ways_by_id()` borrow the encapsulated `pub(crate)` fields (`&HashMap<i64, OsmNode>`, `&[OsmWay]`, `&HashMap<i64, usize>`). 0.2.0 introduced the encapsulation but shipped no public way to read these fields, so downstream consumers (`osm-to-bedrock`, `osm-world`) could not resolve way node references or multipolygon relation members. Purely additive; no behavioral change.

## [0.2.0] - 2026-07-18

Audit-remediation release. The `0.1.2` version was never published (crates.io
remained on `0.1.1`), so every change since `0.1.1` — the audit remediation
plus the structural work that was staged as `0.1.2` — is folded into this
single `0.2.0` entry. Items are grouped by Keep-a-Changelog section; breaking
changes are tagged **Breaking —** inline and require downstream `osm-to-bedrock`
/ `osm-world` to adapt.

### Added

- **Overture cache version + TTL (ARC-001).** `OvertureCacheMeta` now records
  the `overturemaps` CLI version that wrote each entry and a written-at
  timestamp, and the CLI version is folded into the cache key so a CLI upgrade
  invalidates older entries. `OvertureParams.cache_ttl_secs` configures the
  freshness window (`None` selects the default ~30-day TTL, `Some(0)` disables
  the cache, any other `Some(secs)` is honored verbatim).
- **Centralized, deterministic synthetic IDs (ARC-004 / ARC-009 / QA-010 /
  QA-013).** A new `synthetic_ids` module owns the named negative-ID ranges
  used by both the XML writer and the Overture parser, with a compile-time
  assertion that the ranges do not overlap. Overture parsing is now
  deterministic: two parses of the same GeoJSON yield the same IDs.
- **Streaming `parse_osm_xml_file` (ARC-013).** Reads large `.osm` files via a
  `quick-xml` `BufReader` instead of loading the whole document into memory,
  bounding peak memory on 200 MB urban extracts.
- **Single-pass XML parser (ARC-006).** `parse_osm_xml_str` collects nodes,
  ways, and relations in one `read_event` loop instead of tokenizing the whole
  document twice, and now applies an explicit element-depth limit (SEC-004).
- **O(n·k) POI dedupe (ARC-002 / QA-002 / QA-014).**
  `dedupe_pois_with_overture_preference` snaps each POI to a ~25 m spatial grid
  and compares against the cell and its eight neighbors, replacing the previous
  O(n²) nested loop. `poi_duplicates` borrows instead of allocating per
  comparison.
- **O(1) way-id writer lookup (ARC-003 / QA-001).** `write_osm_xml_string`
  builds a one-shot inverse index before iterating ways, replacing the per-way
  linear scan of `ways_by_id` (previously O(W²)).
- **`OsmData` encapsulation accessors.** `OsmData::new`, `push_way`,
  `iter_ways`, `way_id_at`, and `validate_invariants` expose the encapsulated
  ways index without letting external code put it out of sync. The last runs
  automatically under `debug_assertions` from `new` / `push_way`.
- **Generic `RawCache<Meta>` helper (QA-003).** The shared atomic
  write-then-rename cache protocol is extracted into
  `cache_store::RawCache`, used by both `osm_cache` and the Overture cache so
  the fix lives in one place.
- **CI hardening (ARC-014 / ARC-021 / DOC-016).** CI now runs an MSRV-check job
  (pinned to the declared `1.88`), a `cargo doc --no-deps -D warnings` job, a
  `cargo audit` job, a docs-lint job (markdownlint-cli2 + lychee), and an
  ubuntu/macos/windows matrix for the lint and test jobs.
- **CHANGELOG.md and CONTRIBUTING.md (DOC-002 / DOC-003).**
- **Criterion-ready Makefile target (`make bench`) and `.pre-commit-config.yaml`
  wiring `gitleaks` + `detect-private-key` + the Make-backed `fmt` / `lint` /
  `typecheck` hooks (ARC-015 / SEC-009).**

### Changed

- **Breaking — `OsmData` API (ARC-008).** `OsmData` fields are now `pub(crate)`.
  Construction goes through `OsmData::new` and incremental mutation through
  `push_way`, so the `ways` / `ways_by_id` invariant can no longer be broken by
  external callers. Downstream code that constructed `OsmData` with struct
  literals or mutated `ways` directly must migrate to the new constructors and
  accessors.
- **Breaking — `OsmWay` gained a required `id` field (QA-021).** Every way now
  carries its OSM id, which lets `push_way` update `ways_by_id` in O(1) and
  obsoletes the writer's inverse-lookup reverse scan. Downstream construction
  of `OsmWay` must supply the id.
- **Breaking — cache migration is now explicit (ARC-005 / QA-008).** The
  `overpass_cache_dir`, `srtm_cache_dir`, and `overture_cache_dir` getters (and
  the `osm_cache::cache_dir` / `srtm::cache_dir` wrappers) are now pure path
  resolution and never move files. Consumers **must** call
  `cache::migrate_legacy_caches` once at startup to relocate legacy
  `osm-to-bedrock` caches. Migration still targets the shared default location
  regardless of any `PAR_OSM_*_CACHE_DIR` / `*_CACHE_DIR` override; override
  directories are never touched.
- **Breaking — Cargo feature flags (ARC-012).** A `blocking` feature (default
  on) now gates the `reqwest`-based fetch surface — the `overpass` and `srtm`
  modules plus the Overture CLI orchestration in `overture::cli`. Consumers
  wanting only the pure subset (data model, parsing, writing, cache I/O,
  filter, synthetic IDs, elevation) opt out with `default-features = false`.
- **Breaking — `default_overpass_url` return type.** Now returns
  `Cow<'static, str>` instead of leaking a `Box<str>` (ARC-010 follow-up).
- **Breaking — Overture cache API.** `overture_cache_key_with_version`,
  `overture_cache_read`, and `overture_cache_write` gained `cli_version` and
  `ttl` parameters to support the version/TTL awareness (ARC-001).
- **Module split (ARC-007 / QA-009).** `osm.rs` is split into
  `osm/{model,pbf,xml_parse,xml_write}.rs` and `overture.rs` into
  `overture/{theme,parse,cache,cli}.rs`. All original `crate::osm::*` and
  `crate::overture::*` paths continue to resolve via re-exports, so external
  call sites are unaffected.
- **Atomic meta-first cache writes (QA-012).** The shared `RawCache` write
  protocol writes metadata first and finalizes the data file last, so a crash
  no longer leaves an orphan data file that `read` would return but `list`
  skips.
- **Windows `LOCALAPPDATA` precedence (QA-020).** `platform_cache_root` prefers
  `LOCALAPPDATA` over `HOME` on Windows, matching native Windows app
  conventions instead of the MSYS/Cygwin/Git-Bash `HOME`.
- **Live `OVERPASS_URL` read (ARC-010 / QA-017).** `default_overpass_url` reads
  the env var on each call rather than freezing it at first use inside a
  `OnceLock`.
- **`missing_docs` enforced (DOC-007).** Every public item now carries a doc
  comment, `#![warn(missing_docs)]` is enabled, and the crate carries
  `#![doc(html_root_url = "https://docs.rs/par-osm-rust/0.2.0")]`. *(Correction
  2026-07-18: `html_root_url` only sets the base URL for cross-crate rustdoc
  links — it does not control what renders on docs.rs. The README is shown on
  docs.rs because `README.md` is conventionally rendered as the crate root
  page, and the same README renders as the front page on crates.io. DOC-010.)*

### Fixed

- **Overpass redirect following (SEC-002).** Overpass `reqwest` clients are
  built with `redirect(reqwest::redirect::Policy::none())`. The previous
  default followed up to ten redirects, allowing an allowlisted mirror to
  bypass the `validate_overpass_url` SSRF host allowlist via a `302` to an
  internal host. Any `3xx` is now treated as an error.
- **Overpass error bodies capped (SEC-005).** Surfaced Overpass error bodies
  are truncated before entering the `anyhow` error chain, preventing an
  unbounded response body from being held in memory or logged.
- **SRTM redirect policy (SEC-003).** The SRTM downloader also sets
  `redirect(Policy::none())` for consistency; the download URL remains a
  hardcoded constant plus an integer-derived tile name, so the redirect
  surface was already bounded.
- **Overpass port pinned to 443 (SEC-011).** `validate_overpass_url` rejects
  non-443 ports on allowlisted hosts so an attacker controlling a mirror
  cannot redirect to an alternate port on the same host.
- **`reqwest::blocking::Client` pooling (ARC-020).** Overpass and SRTM reuse a
  pooled client instead of rebuilding one per fetch.
- **Dangling-node-ref writer validation (ARC-016).** The XML writer rejects
  ways that reference absent node IDs instead of silently emitting `<nd ref>`
  entries that produce structurally invalid XML on round-trip.
- **Cache migration symlink-skip + streamed `files_equal` (SEC-006 / QA-016).**
  Migration uses `symlink_metadata` and skips symlinks (closing a local
  data-exfil / OOM vector via crafted symlink targets), and `files_equal`
  streams both files with early exit instead of loading them fully into memory.
- **Overture CLI argument-injection guard (SEC-012).** `fetch_geojson_for_type`
  rejects `cli_type` values starting with `-` or containing whitespace. A new
  `PAR_OSM_OVERTURE_CLI` environment variable lets callers pin an absolute
  `overturemaps` executable path, closing a PATH-lookup hijack vector in
  multi-user setups (SEC-010).

### Security

- Closed the redirect-following gap that undermined the `validate_overpass_url`
  SSRF allowlist (SEC-002; see **Fixed**), pinned the Overpass port to 443
  (SEC-011), and disabled SRTM redirect following (SEC-003).
- Added `.claude/settings.local.json`, `.env`, `*.pem`, `*.key`, and
  `secrets.*` to `.gitignore` as defense-in-depth (SEC-001 / SEC-009), and
  wired `gitleaks` + `detect-private-key` into pre-commit so credentials
  cannot be committed silently inside a larger change.

### Removed

- Nothing was removed. The legacy `osm_cache` family (`cache_key`, `read`,
  `write`, `find_containing`) is now `#[deprecated]` (ARC-011 / QA-011) in
  favor of the URL-aware `*_for_url` family, so existing call sites keep
  working with a warning rather than breaking.

## [0.1.1] - 2026-05-10

### Added

- `Makefile` with the standard target set: `build`, `build-release`, `test`,
  `lint`, `fmt`, `fmt-check`, `typecheck`, `check`, `checkall`, `bench`,
  `pre-commit`, and `clean`. `checkall` mirrors what CI runs.
- `source_options` parsers donated from `osm-to-bedrock` (ARC-011), turning
  CLI/config strings into `OvertureTheme`, `ThemePriority`, and related
  source-selection enums so downstream applications do not reimplement them.

### Changed

- Bumped `quick-xml` `0.39` → `0.41` and migrated to the new `normalized_value`
  API on the XML parser's attribute readers.
- Bumped `reqwest` `0.12` → `0.13`.
- Bumped `sha2` `0.10` → `0.11`.

## [0.1.0] - 2026-05-03

Initial release of `par-osm-rust` as a shared data-source crate extracted from
`osm-to-bedrock` and `osm-world`. Ships Overpass fetching with SSRF host
allowlisting, optional Overture Maps integration via the `overturemaps` CLI,
OSM XML/PBF parsing, normalized `OsmData` interchange, SRTM tile download, HGT
elevation sampling, atomic write-then-rename cache discipline, and the
`sources::fetch_map_data` orchestration entry point.

[Unreleased]: https://github.com/paulrobello/par-osm-rust/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/paulrobello/par-osm-rust/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/paulrobello/par-osm-rust/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/paulrobello/par-osm-rust/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/paulrobello/par-osm-rust/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/paulrobello/par-osm-rust/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/paulrobello/par-osm-rust/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/paulrobello/par-osm-rust/releases/tag/v0.1.0
