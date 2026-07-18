# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Released versions are published to [crates.io](https://crates.io/crates/par-osm-rust).

## [Unreleased]

Audit-remediation wave landing ahead of the planned `0.2.0` bump. Items are
grouped by Keep-a-Changelog section. **Breaking changes** are tagged inline and
collected under **Changed** and **Removed**; they are the reason the next
release will be `0.2.0` rather than a patch.

### Added

- Pre-commit configuration (`.pre-commit-config.yaml`) wiring `gitleaks`,
  `detect-private-key`, and the Make-backed `fmt` / `lint` / `typecheck` hooks.
- Expanded CI: an MSRV-check job (verified against the declared `1.87`), a
  `cargo doc --no-deps -D rustdoc::broken_intra_doc_links` job, and a
  `cargo audit` job for vulnerable transitive dependencies.
- New `OsmData` accessors exposing the encapsulated ways index without letting
  external code put it out of sync: `OsmData::new`, `push_way`, `iter_ways`,
  `way_id_at`, and `validate_invariants` (the last is called automatically in
  debug builds).

### Changed

- **Breaking — `OsmData` API.** `OsmData` fields are no longer all `pub`.
  Construction goes through `OsmData::new` and incremental mutation through
  `push_way`, so the `ways` / `ways_by_id` invariant can no longer be broken by
  external callers. Downstream code that constructed `OsmData` with struct
  literals or mutated `ways` directly must migrate to the new constructors and
  accessors.
- **Breaking — cache migration is now explicit.** The `overpass_cache_dir`,
  `srtm_cache_dir`, and `overture_cache_dir` getters (and the
  `osm_cache::cache_dir` / `srtm::cache_dir` wrappers) are pure path resolution
  and never move files. Consumers **must** call `cache::migrate_legacy_caches`
  once at startup to relocate legacy `osm-to-bedrock` caches. Migration still
  targets the shared default location and never touches `PAR_OSM_*_CACHE_DIR` /
  `*_CACHE_DIR` override directories.
- **Breaking — Cargo feature flags.** Fetch modules now sit behind
  `default = ["blocking"]`. Async-only consumers can opt out with
  `default-features = false`. Downstream manifests that rely on the implicit
  fetch surface must add the `blocking` feature (on by default).
- Performance: `dedupe_pois_with_overture_preference` now uses a spatial grid
  (snap to ~25 m cell, compare against the cell and its eight neighbors) instead
  of the previous O(n²) nested loop, and `poi_duplicates` borrows instead of
  allocating per comparison.
- Performance: `parse_osm_xml_str` is now single-pass — nodes, ways, and
  relations are collected in one `read_event` loop instead of tokenizing the
  whole document twice.
- Performance: `write_osm_xml_string` builds a one-shot inverse
  `HashMap<usize, i64>` before iterating ways, replacing the per-way linear
  scan of `ways_by_id` (previously O(W²)).

### Fixed

- Overpass `reqwest` clients are now built with
  `redirect(reqwest::redirect::Policy::none())`. The previous default followed
  up to ten redirects, allowing an allowlisted mirror to bypass the host
  allowlist via a `302` to an internal host. Any `3xx` is now treated as an
  error.
- Overpass error bodies are now capped before being surfaced into the `anyhow`
  error chain, preventing an unbounded response body from being held in memory
  or logged.

### Security

- Closed the redirect-following gap that undermined the
  `validate_overpass_url` SSRF allowlist (see **Fixed**).
- Added `.claude/settings.local.json`, `.env`, `*.pem`, `*.key`, and
  `secrets.*` to `.gitignore` as defense-in-depth, and wired `gitleaks` into
  pre-commit so credentials cannot be committed silently.

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

[Unreleased]: https://github.com/paulrobello/par-osm-rust/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/paulrobello/par-osm-rust/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/paulrobello/par-osm-rust/releases/tag/v0.1.0
