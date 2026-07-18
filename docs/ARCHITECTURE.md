# Architecture

System design and module organization for `par-osm-rust` — a shared Rust crate for fetching, caching, parsing, and normalizing OpenStreetMap-compatible map data.

## Table of Contents

- [Overview](#overview)
- [System Architecture](#system-architecture)
- [Module Layout](#module-layout)
- [Source Orchestration](#source-orchestration)
- [Cache Architecture](#cache-architecture)
- [Data Model](#data-model)
- [Overpass Integration](#overpass-integration)
- [Overture Maps Integration](#overture-maps-integration)
- [Elevation and SRTM](#elevation-and-srtm)
- [Security Boundaries](#security-boundaries)
- [Operational Notes](#operational-notes)
- [Tradeoffs](#tradeoffs)
- [Related Documentation](#related-documentation)

## Overview

**Purpose:** `par-osm-rust` gives OSM-consuming applications one shared data-source layer for Overpass, optional Overture Maps data, raw cache management, OSM XML/PBF parsing, SRTM downloads, and HGT elevation lookup.

**Key design goals:**

- Keep source fetching, cache layout, and parsing behavior consistent across downstream projects.
- Keep application-specific rendering, UI, Minecraft, WGPU, and game logic out of the crate.
- Make source orchestration explicit: OSM is the baseline source; Overture is opt-in and policy-driven.
- Preserve safe defaults for network access, cache migration, and fallback behavior.

**Tech stack:** Rust 2024 (MSRV 1.88), `osmpbf`, `quick-xml`, `serde`, and SRTM HGT files. The default `blocking` Cargo feature pulls in blocking `reqwest` for the Overpass, SRTM, and Overture-CLI orchestration paths; `default-features = false` compiles only the pure subset (data model, parsers, writer, cache I/O, filter, synthetic IDs, elevation). The `overturemaps` CLI remains an optional runtime dependency, never a compile-time one.

## System Architecture

`par-osm-rust` sits between applications and external map/elevation providers. Applications call high-level source APIs when they want shared policy, or lower-level modules when they need direct control.

```mermaid
graph TD
    App[Downstream Application]
    Sources[sources::fetch_map_data]
    Overpass[overpass]
    Overture[overture]
    OsmCache[osm_cache]
    SharedCache[cache]
    Osm[osm]
    Srtm[srtm]
    Elevation[elevation]
    OverpassApi[Overpass API]
    OvertureCli[overturemaps CLI]
    HgtFiles[(SRTM HGT Files)]
    OsmData[osm::OsmData]

    App --> Sources
    App --> Overpass
    App --> Osm
    App --> Srtm
    App --> Elevation

    Sources --> Overpass
    Sources --> Overture
    Sources --> OsmData

    Overpass --> OsmCache
    Overpass --> OverpassApi
    Overpass --> Osm

    Overture --> SharedCache
    Overture --> OvertureCli
    Overture --> OsmData

    Srtm --> SharedCache
    Srtm --> HgtFiles
    Elevation --> HgtFiles

    Osm --> OsmData

    class App external
    class Sources primary
    class Overpass,Overture,Osm,Srtm,Elevation active
    class OsmCache,SharedCache data
    class OverpassApi,OvertureCli external
    class HgtFiles database
    class OsmData data

    classDef primary fill:#e65100,stroke:#ff9800,stroke-width:3px,color:#ffffff
    classDef active fill:#1b5e20,stroke:#4caf50,stroke-width:2px,color:#ffffff
    classDef data fill:#0d47a1,stroke:#2196f3,stroke-width:2px,color:#ffffff
    classDef database fill:#1a237e,stroke:#3f51b5,stroke-width:2px,color:#ffffff
    classDef external fill:#4a148c,stroke:#9c27b0,stroke-width:2px,color:#ffffff
```

## Module Layout

| Module | Responsibility |
| --- | --- |
| `lib.rs` | Public module exports, crate-level usage docs, and the `#![warn(missing_docs)]` gate |
| `filter.rs` | `FeatureFilter` flags that control Overpass query categories |
| `overpass.rs` | Overpass URL validation, query construction, HTTP fetch, and cache-aware OSM fetch. Gated behind the default `blocking` feature. Exposes `default_overpass_url() -> Cow<'static, str>` (reads `OVERPASS_URL` live, else a baked-in default). |
| `osm_cache.rs` | Endpoint-aware raw Overpass XML cache keys, reads, writes, and containment lookups. The legacy bbox+filter family (`cache_key`, `read`, `write`, `find_containing`) is `#[deprecated]` in favor of the URL-aware `_for_url` variants. |
| `cache_store.rs` | Generic `RawCache<Meta>` disk cache (data file + JSON sidecar) with an atomic write protocol; the shared implementation underneath `osm_cache` and `overture`. |
| `overture/` | Optional `overturemaps` CLI integration, GeoJSON cache, and Overture-to-OSM normalization. Split into `theme`, `parse`, `cache`, and `cli` submodules; `cli` is gated behind `blocking`. |
| `sources.rs` | High-level OSM/Overture orchestration, fallback policy, POI modes, and dedupe |
| `osm/` | Normalized OSM data model, XML/PBF parsing, merging, bbox clipping, and XML serialization. Split into `model`, `pbf`, `xml_parse` (single-pass plus streaming `parse_osm_xml_file`), and `xml_write` submodules. |
| `cache.rs` | Shared cache root resolution and the `migrate_legacy_caches()` startup migration. Pure path getters never migrate. |
| `srtm.rs` | SRTM tile naming, tile discovery, downloading, and cache location. Gated behind `blocking`. |
| `elevation.rs` | HGT tile parsing and bilinear elevation sampling |
| `synthetic_ids.rs` | Deterministic, non-overlapping negative-ID allocators for the OSM writer and Overture normalization, each in its own billion-wide band |
| `source_options.rs` | CLI/config string parsers into `OvertureTheme`, `ThemePriority`, `PoiSourceMode`, and `OvertureFailureMode` |

## Source Orchestration

Use `sources::fetch_map_data` when an application wants the shared policy for OSM plus optional Overture data.

```mermaid
graph TD
    Start[fetch_map_data]
    Resolve[Resolve SourceOptions]
    FetchOsm[Fetch OSM through overpass::fetch_osm_data]
    OvertureEnabled{Overture enabled?}
    FetchOverture[Fetch requested Overture themes]
    OvertureFailed{Overture failed?}
    FailureMode{Failure mode}
    Merge[merge_source_data]
    ReturnFallback[Return OSM data with warning]
    ReturnError[Return error]
    Result[SourceFetchResult]

    Start --> Resolve
    Resolve --> FetchOsm
    FetchOsm --> OvertureEnabled
    OvertureEnabled -- no --> Merge
    OvertureEnabled -- yes --> FetchOverture
    FetchOverture --> OvertureFailed
    OvertureFailed -- no --> Merge
    OvertureFailed -- yes --> FailureMode
    FailureMode -- FallbackToOsm --> ReturnFallback
    FailureMode -- Fail --> ReturnError
    ReturnFallback --> Result
    Merge --> Result

    class Start,Result primary
    class Resolve,FetchOsm,FetchOverture,Merge active
    class OvertureEnabled,OvertureFailed,FailureMode warning
    class ReturnFallback warning
    class ReturnError error

    classDef primary fill:#e65100,stroke:#ff9800,stroke-width:3px,color:#ffffff
    classDef active fill:#1b5e20,stroke:#4caf50,stroke-width:2px,color:#ffffff
    classDef warning fill:#ff6f00,stroke:#ffa726,stroke-width:2px,color:#ffffff
    classDef error fill:#b71c1c,stroke:#f44336,stroke-width:2px,color:#ffffff
```

`SourceOptions::default()` performs an OSM-only fetch. `PoiSourceMode::OverturePreferred` is the default POI policy, but it does not trigger Overture fetching unless `OvertureParams.enabled` is `true`.

### POI merge policy

`merge_source_data` handles already-loaded data without network or cache I/O. It applies the selected `PoiSourceMode` and dedupes near-duplicate OSM/Overture points by category, normalized name, and distance.

| Mode | Result |
| --- | --- |
| `OsmOnly` | Keep OSM POIs; non-POI Overture geometry can still merge when supplied. |
| `OvertureOnly` | Use Overture POIs; clear OSM POIs if Overture is unavailable. |
| `Both` | Merge OSM and Overture POIs, preferring Overture representatives for duplicates. |
| `OverturePreferred` | Use Overture POIs when present; fall back to OSM POIs otherwise. |

## Cache Architecture

The crate separates cache root resolution from data-specific cache behavior.

```mermaid
graph LR
    Env[Environment Overrides]
    Shared[Shared Defaults]
    Legacy[Legacy osm-to-bedrock Caches]
    Cache[cache.rs]
    OverpassDir[Overpass XML Cache]
    OvertureDir[Overture GeoJSON Cache]
    SrtmDir[SRTM HGT Cache]

    Env --> Cache
    Shared --> Cache
    Legacy --> Cache
    Cache --> OverpassDir
    Cache --> OvertureDir
    Cache --> SrtmDir

    class Cache primary
    class Env,Shared active
    class Legacy warning
    class OverpassDir,OvertureDir,SrtmDir data

    classDef primary fill:#e65100,stroke:#ff9800,stroke-width:3px,color:#ffffff
    classDef active fill:#1b5e20,stroke:#4caf50,stroke-width:2px,color:#ffffff
    classDef warning fill:#ff6f00,stroke:#ffa726,stroke-width:2px,color:#ffffff
    classDef data fill:#0d47a1,stroke:#2196f3,stroke-width:2px,color:#ffffff
```

Default cache locations live under `~/.cache/par-osm-rust`. Environment overrides take precedence for each data type:

| Data | Resolution priority |
| --- | --- |
| Overpass XML | `PAR_OSM_OVERPASS_CACHE_DIR`, then `OVERPASS_CACHE_DIR`, then shared default |
| Overture GeoJSON | `PAR_OSM_OVERTURE_CACHE_DIR`, then `OVERTURE_CACHE_DIR`, then shared default |
| SRTM HGT | `PAR_OSM_SRTM_CACHE_DIR`, then `SRTM_CACHE_DIR`, then shared default |

Legacy `osm-to-bedrock` cache directories are migrated into the shared default location only when an application explicitly calls `cache::migrate_legacy_caches()` once at startup. The `overpass_cache_dir`, `srtm_cache_dir`, and `overture_cache_dir` getters are pure path resolution and never migrate — keeping accessors side-effect-free and eliminating the first-call race between concurrent callers. Migration always targets the default shared location regardless of any `PAR_OSM_*_CACHE_DIR` / `*_CACHE_DIR` override; override directories are never touched.

Both raw caches (`osm_cache` for Overpass XML and `overture` for GeoJSON) layer on top of `cache_store::RawCache<Meta>`: a generic data-file-plus-JSON-sidecar store with an atomic write protocol that finalizes the meta sidecar first and renames the data payload last, so the only committed state is "both files present." The Overture cache additionally stamps each entry with the `overturemaps` CLI version that wrote it and offers a TTL (default ~30 days via `OvertureParams::cache_ttl_secs`); entries whose age exceeds the TTL, or that were written under a different CLI version, are treated as misses and re-fetched.

## Data Model

`osm::OsmData` is the normalized interchange format for downstream applications.

| Field | Purpose |
| --- | --- |
| `nodes` | OSM and synthetic node coordinates keyed by ID |
| `ways` | Ordered way geometry with tags |
| `ways_by_id` | Lookup index for relation member resolution |
| `relations` | Multipolygon relations and member roles |
| `bounds` | Optional dataset bounding box |
| `poi_nodes` | Renderable points of interest |
| `addr_nodes` | Standalone address nodes |
| `tree_nodes` | Individual tree points |

`osm::FeatureSource` tracks whether features came from OSM, Overture, or synthetic generation. Overture geometry receives synthetic negative node IDs to avoid collisions with real OSM IDs.

`OsmData` is encapsulated: its collections are `pub(crate)`, so external consumers construct instances through `OsmData::new` and extend them with `push_way`, then read back through the `iter_ways`, `way_id_at`, and `validate_invariants` accessors. `OsmWay` carries its own `pub id: i64` as the single source of truth consumed by the writer and `ways_by_id` — the prior `(id, way)` pair plumbing is gone. `parse_osm_xml_file` streams large `.osm` extracts from disk via a `BufReader` so peak memory stays bounded.

## Overpass Integration

`overpass.rs` owns the Overpass HTTP boundary.

**Responsibilities:**

- Validate endpoint URLs before network access.
- Build Overpass QL from a bounding box and `FeatureFilter`.
- Send blocking HTTP requests with a crate-specific user agent.
- Return readable errors for busy or failing Overpass responses.
- Use URL-aware cache keys so different Overpass endpoints do not share stale raw XML.
- Expose `default_overpass_url() -> Cow<'static, str>` so callers read the `OVERPASS_URL` override live on every call rather than freezing the endpoint at startup; returns a borrow of the baked-in default when unset.

Applications that run async event loops should call Overpass fetches from a blocking worker thread.

## Overture Maps Integration

`overture` treats Overture as an optional runtime integration. The Rust crate does not depend on the Python package at compile time, and the entire `cli` submodule (subprocess invocation and high-level fetch orchestration) is gated behind the default `blocking` Cargo feature. The `theme`, `parse`, and `cache` submodules remain available in the pure subset so GeoJSON normalization and cache I/O work without a network stack.

**Runtime boundary:**

1. Check availability with `overture::is_cli_available()`.
2. Invoke `overturemaps download` for requested themes.
3. Cache GeoJSON output under the Overture cache directory.
4. Normalize Overture features into `OsmData`.
5. Let `sources.rs` apply source and POI policy.

> **Note:** Missing Overture support is not an error when callers use `OvertureFailureMode::FallbackToOsm`. The returned `SourceFetchResult` carries warnings so applications can surface degraded behavior without failing the whole fetch.

## Elevation and SRTM

`srtm.rs` maps a geographic bounding box to the needed SRTM tiles, downloads missing `.hgt` files, and resolves the SRTM cache directory. `elevation.rs` reads HGT data and samples elevations.

The elevation path is intentionally independent from OSM source orchestration. Callers decide whether terrain elevation is required for their workload.

## Security Boundaries

| Boundary | Protection |
| --- | --- |
| Overpass URL | HTTPS-only, no userinfo, approved host allowlist |
| Cache path overrides | Environment variables select directories, but no cache content is trusted as executable code |
| Overture CLI | Optional external process with per-command timeout and stderr truncation |
| Synthetic IDs | Distinct billion-wide negative-ID bands per allocator (writer nodes, writer ways, writer relations, Overture) never overlap each other or positive OSM IDs |
| Secrets | The crate does not require secrets for normal fetch, parse, cache, or publish-time operation |

## Operational Notes

- Keep Overture disabled by default unless a caller explicitly opts in.
- Prefer `sources::fetch_map_data` for application-facing source policy so downstream projects stay consistent.
- Use endpoint-aware cache helpers (`osm_cache::*_for_url`, versioned `overture_cache_key_with_version`) for new fetch paths.
- The declared MSRV is 1.88; CI verifies the build on that toolchain. Build with `--no-default-features` for the pure parse/model/cache subset (no `reqwest`).
- Run `make checkall` (format check, clippy with `-D warnings`, `cargo check`, and the full test suite) before committing; CI runs the same gate on Ubuntu, macOS, and Windows plus dedicated MSRV, docs, security-audit, and docs-lint jobs.
- Run `cargo publish --dry-run` before publishing a new crate version.
- Use the manual GitHub Actions publishing workflow rather than automatic publish-on-push.

## Tradeoffs

| Choice | Benefit | Cost |
| --- | --- | --- |
| Blocking HTTP API | Simple synchronous crate API for CLI and renderer setup code | Async applications must use blocking worker threads |
| Optional Overture CLI | Avoids a heavy compile-time Overture dependency | Runtime availability and CLI failures must be handled |
| URL-aware raw XML cache | Prevents stale cross-endpoint cache reuse | Cache keys include more inputs and require endpoint metadata |
| Shared cache root | Lets multiple projects reuse downloads | Requires legacy migration and override precedence rules |
| `OsmData` as interchange format | Downstream projects consume one normalized model | Some source-specific details are intentionally flattened |

## Related Documentation

- [Project README](../README.md) - Usage, examples, cache behavior, and verification commands
- [Documentation Style Guide](DOCUMENTATION_STYLE_GUIDE.md) - Documentation structure, tone, and diagram standards
- [Crates.io publishing workflow](../.github/workflows/publish-crates.yml) - Manual release workflow
- [CI workflow](../.github/workflows/ci.yml) - Format, lint, and test checks
