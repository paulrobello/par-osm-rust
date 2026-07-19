# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Released versions are published to [crates.io](https://crates.io/crates/par-osm-rust).

## [Unreleased]

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
  `#![doc(html_root_url = "https://docs.rs/par-osm-rust/0.2.0")]` so docs.rs
  renders the README front page (DOC-012).

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

[Unreleased]: https://github.com/paulrobello/par-osm-rust/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/paulrobello/par-osm-rust/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/paulrobello/par-osm-rust/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/paulrobello/par-osm-rust/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/paulrobello/par-osm-rust/releases/tag/v0.1.0
