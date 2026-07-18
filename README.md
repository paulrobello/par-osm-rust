# par-osm-rust

[![CI](https://github.com/paulrobello/par-osm-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/paulrobello/par-osm-rust/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/par-osm-rust.svg)](https://crates.io/crates/par-osm-rust)
[![Docs.rs](https://docs.rs/par-osm-rust/badge.svg)](https://docs.rs/par-osm-rust)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust MSRV](https://img.shields.io/badge/rust-1.87%2B-orange.svg)](https://www.rust-lang.org)

Shared Rust utilities for fetching, caching, parsing, and normalizing OpenStreetMap-compatible map data.

`par-osm-rust` is the data-source crate used by `osm-to-bedrock` and `osm-world`. It owns network and cache concerns only: Overpass/OSM fetching, optional Overture Maps fetching, source merge policy, OSM XML/PBF parsing, SRTM tile downloads, and HGT elevation lookup. It intentionally does **not** depend on Minecraft, WGPU, UI, renderer, or application-specific types.

```toml
par-osm-rust = "0.2.0"
```

For local workspace development, use a path dependency instead:

```toml
par-osm-rust = { path = "../par-osm-rust" }
```

Consumers that want only the pure subset (data model, parsing, writing, cache I/O, filter, synthetic IDs, elevation) without the blocking `reqwest` fetch surface can disable default features:

```toml
par-osm-rust = { version = "0.2.0", default-features = false }
```

## What it provides

- **OSM / Overpass**
  - Safe Overpass endpoint validation with an approved HTTPS host allowlist.
  - Overpass QL query generation from a bounding box and [`FeatureFilter`](src/filter.rs).
  - URL-aware raw Overpass XML cache keys, so different endpoints do not share stale raw XML.
- **Overture Maps**
  - Optional runtime integration with the `overturemaps` Python CLI.
  - Theme selection for buildings, transportation, places, base land/water, and addresses.
  - GeoJSON-to-`OsmData` normalization with synthetic negative IDs for non-OSM geometry.
- **Source orchestration**
  - One high-level fetch path, `sources::fetch_map_data`, for OSM-only, Overture-only, merged, and Overture-preferred POI modes.
  - Configurable Overture failure behavior: graceful OSM fallback by default, or strict failure.
  - POI dedupe for near-duplicate OSM/Overture points, preferring Overture representatives.
- **Parsing / serialization**
  - OSM PBF parsing (`osm::parse_pbf`).
  - OSM XML parsing from a string (`osm::parse_osm_xml_str`) or a streaming file path (`osm::parse_osm_xml_file`) for nodes, ways, relations, POIs, addresses, trees, and bounds.
  - Normalized OSM XML writing via `osm::write_osm_xml_string`.
- **Elevation**
  - SRTM tile naming, download, cache lookup, and HGT elevation sampling.
- **Cache migration**
  - Shared cache locations under `par-osm-rust`.
  - Legacy cache migration from older `osm-to-bedrock` cache directories.

## Quick start: fetch normalized map data

Use `sources::fetch_map_data` when an application wants the shared OSM/Overture source policy instead of manually fetching each source.

```rust,no_run
use par_osm_rust::filter::FeatureFilter;
use par_osm_rust::overture::{OvertureParams, OvertureTheme};
use par_osm_rust::sources::{
    fetch_map_data, OvertureFailureMode, PoiSourceMode, SourceOptions,
};

fn main() -> anyhow::Result<()> {
    let bbox = (38.0, -121.0, 38.01, -120.99); // south, west, north, east

    let options = SourceOptions {
        filter: FeatureFilter::default(),
        overpass_url: None,       // None uses overpass::default_overpass_url()
        use_overpass_cache: true,
        overture: OvertureParams {
            enabled: true,        // Overture is never fetched unless enabled is true
            themes: vec![OvertureTheme::Place],
            ..OvertureParams::default()
        },
        poi_source_mode: PoiSourceMode::OverturePreferred,
        overture_failure_mode: OvertureFailureMode::FallbackToOsm,
    };
    let mut progress = |_: f32, _: &str| {};
    let result = fetch_map_data(bbox, &options, &mut progress)?;

    println!("status: {:?}", result.status);
    println!("pois: {}", result.data.poi_nodes.len());
    for warning in result.warnings {
        eprintln!("warning: {warning}");
    }

    Ok(())
}
```

If you only need OSM/Overpass data, use `overpass::fetch_osm_data` directly:

```rust,no_run
use par_osm_rust::{filter::FeatureFilter, overpass};

fn main() -> anyhow::Result<()> {
    let bbox = (38.0, -121.0, 38.01, -120.99);
    let url = overpass::default_overpass_url();
    let data = overpass::fetch_osm_data(bbox, &FeatureFilter::default(), true, url)?;
    println!("ways: {}", data.iter_ways().count());
    Ok(())
}
```

## Source options and POI modes

`SourceOptions::default()` uses:

- `FeatureFilter::default()`
- default Overpass URL resolution
- `use_overpass_cache = true`
- `OvertureParams::default()` (`enabled = false`)
- `PoiSourceMode::OverturePreferred`
- `OvertureFailureMode::FallbackToOsm`

Important nuance: `PoiSourceMode::OverturePreferred` is the default policy, but Overture is fetched only when `SourceOptions.overture.enabled == true`. With default options, `fetch_map_data` performs an OSM/Overpass fetch only.

`OvertureParams::default()` field values:

| Field | Default | Notes |
| --- | --- | --- |
| `enabled` | `false` | Overture is never fetched unless this is `true`. |
| `themes` | `OvertureTheme::all()` | Every supported theme. |
| `priority` | empty map | Per-theme source priority; missing entries resolve to `ThemePriority::Both` via `priority_for(theme)`. |
| `timeout_secs` | `120` | Per `overturemaps download` CLI invocation timeout. |
| `cache_ttl_secs` | `None` | `None` selects the ~30-day default TTL, `Some(0)` disables the Overture cache, any other `Some(secs)` is honored verbatim. |

`ThemePriority` controls which source wins when OSM and Overture both cover the same **non-POI** theme (POIs go through the `PoiSourceMode` policy above):

| Variant | Behavior |
| --- | --- |
| `Overture` | Prefer Overture features for this theme. |
| `Osm` | Prefer OSM/Overpass features for this theme. |
| `Both` | Keep features from both sources. This is the default when a theme is not present in `priority`. |

POI source modes:

| Mode | Behavior |
| --- | --- |
| `OsmOnly` | Keep OSM POIs. Overture non-POI data may still merge when explicitly fetched, but OSM POIs win. |
| `OvertureOnly` | Use Overture POIs only; clear OSM POIs if Overture is unavailable. |
| `Both` | Merge OSM and Overture POIs, dedupe near duplicates, prefer Overture representatives. |
| `OverturePreferred` | Use Overture POIs when present; fall back to OSM POIs when Overture is unavailable or returns zero POIs. |

Overture failure modes:

| Mode | Behavior |
| --- | --- |
| `FallbackToOsm` | Return OSM data with warnings when Overture fetch fails. This is the default. |
| `Fail` | Return an error if Overture fetch fails. |

`SourceFetchResult.status` reports the effective outcome (`osm_only`, `overture_only`, `both`, `overture_preferred`, or `overture_fallback_to_osm`) and `warnings` carries human-readable fallback details.

## Overture Maps runtime dependency

Overture integration shells out to the optional `overturemaps` CLI. Install it separately when Overture data is desired:

```bash
python -m pip install overturemaps
```

Use `overture::is_cli_available()` to check availability before presenting Overture options in an application UI. If callers use `sources::fetch_map_data` with `OvertureFailureMode::FallbackToOsm`, a missing or failing CLI becomes a warning and OSM data is returned.

Set the `PAR_OSM_OVERTURE_CLI` environment variable to an absolute path to pin a specific `overturemaps` executable instead of relying on PATH lookup (useful in multi-user setups).

## Cache locations

Default shared cache directories:

- Overpass XML: `~/.cache/par-osm-rust/overpass`
- Overture GeoJSON: `~/.cache/par-osm-rust/overture`
- SRTM HGT: `~/.cache/par-osm-rust/srtm`

Environment override priority:

| Data | Priority |
| --- | --- |
| Overpass XML cache | `PAR_OSM_OVERPASS_CACHE_DIR`, then `OVERPASS_CACHE_DIR`, then shared default |
| Overture GeoJSON cache | `PAR_OSM_OVERTURE_CACHE_DIR`, then `OVERTURE_CACHE_DIR`, then shared default |
| SRTM HGT cache | `PAR_OSM_SRTM_CACHE_DIR`, then `SRTM_CACHE_DIR`, then shared default |
| Overpass endpoint | `OVERPASS_URL`, then `https://overpass-api.de/api/interpreter` |

Legacy caches from the older `osm-to-bedrock` layout can be migrated into the shared default location:

- `~/.cache/osm-to-bedrock/overpass`
- `~/.cache/osm-to-bedrock/overture`
- `~/.cache/osm-to-bedrock/srtm`

**Consumers MUST call [`cache::migrate_legacy_caches`] once at startup** to move these directories. The `overpass_cache_dir`, `srtm_cache_dir`, and `overture_cache_dir` getters (and the `osm_cache::cache_dir` / `srtm::cache_dir` wrappers) are pure path resolution and do **not** migrate anything.

```rust,no_run
fn main() -> anyhow::Result<()> {
    // Call once at startup, before any cache access. Idempotent: a second
    // invocation is a no-op once the legacy directories are empty.
    let report = par_osm_rust::cache::migrate_legacy_caches()?;
    println!(
        "migrated overpass files: {}",
        report.overpass.moved_files + report.overpass.copied_files
    );
    println!(
        "migrated overture files: {}",
        report.overture.moved_files + report.overture.copied_files
    );
    println!(
        "migrated srtm files: {}",
        report.srtm.moved_files + report.srtm.copied_files
    );
    Ok(())
}
```

Migration always targets the default shared location regardless of any `PAR_OSM_*_CACHE_DIR` / `*_CACHE_DIR` override the caller has set. Override directories are never touched by migration.

### Overpass cache isolation

Raw Overpass cache entries are keyed by:

- snapped bounding box (`south, west, north, east` rounded to 4 decimals),
- `FeatureFilter`, and
- canonical Overpass endpoint URL.

Use `osm_cache::cache_key_for_url`, `read_for_url`, `write_for_url`, and `find_containing_for_url` for endpoint-aware raw XML cache operations. The legacy `cache_key`, `read`, `write`, and `find_containing` APIs remain available (now `#[deprecated]`) for backwards compatibility, but new endpoint-aware fetch paths should use the URL-aware helpers.

### Listing and clearing cached areas

Two free functions enumerate or evict entries in the default Overpass XML cache directory without touching override directories:

- `osm_cache::list_areas() -> Vec<CacheEntry>` returns every valid (paired data + metadata) entry with its bbox, filter, timestamp, and size.
- `osm_cache::clear(min_age: Option<chrono::Duration>) -> Result<usize>` deletes entries older than `min_age` (or all entries when `None`) and returns the count removed.

The Overture cache has the equivalent `overture::list_overture_areas()` and `overture::clear_overture_cache(min_age)` helpers.

## Normalized OSM data model

The central type is `osm::OsmData`. The `ways` and `ways_by_id` fields are coupled (every way has exactly one entry in the index mapping its OSM id to its position) and are encapsulated: construction goes through [`osm::OsmData::new`] and incremental mutation through [`osm::OsmData::push_way`], so external code cannot put the pair out of sync. `OsmWay` carries its own `id: i64` field, so the index stays consistent without an inverse lookup. Accessors:

- `new(nodes, ways, relations, bounds, poi_nodes, addr_nodes, tree_nodes)` constructs an `OsmData` and seeds `ways_by_id` from each way's `id`.
- `push_way(way)` appends a way and updates `ways_by_id` atomically.
- `iter_ways()` borrows the ways slice in insertion order.
- `way_id_at(index)` recovers a way's OSM id by index.
- `validate_invariants()` verifies the `ways` / `ways_by_id` pair (called automatically in debug builds from `new`/`push_way`).

Top-level collections:

- `nodes`: OSM or synthetic node coordinates keyed by ID.
- `ways` / `ways_by_id`: ordered node-reference geometry with tags, plus the id→index lookup used for relation member resolution.
- `relations`: multipolygon relations and roles.
- `bounds`: optional dataset bounding box.
- `poi_nodes`: renderable/tagged POI nodes.
- `addr_nodes`: standalone address nodes.
- `tree_nodes`: individual tree points.

`osm::FeatureSource` tracks whether normalized features came from OSM, Overture, or synthetic generation. This is especially useful for POI merge/dedupe behavior.

### Parsing OSM data

Three parse entry points produce an `OsmData`, differing only in input source and memory profile:

| Function | Input | Notes |
| --- | --- | --- |
| `osm::parse_pbf(path: &Path)` | Binary `.osm.pbf` | Memory-maps the file via `osmpbf`; suitable for planet extracts. |
| `osm::parse_osm_xml_str(xml: &str)` | XML string | Single-pass parser; collects nodes, ways, and relations in one `read_event` loop. |
| `osm::parse_osm_xml_file(path: impl AsRef<Path>)` | `.osm` file path | Streams via `BufReader` instead of loading the whole document into memory, bounding peak memory on large extracts. |

All three populate nodes, ways, relations, bounds, POIs, addresses, and trees. The XML parsers handle Overpass/OSM-style XML where ways and relations may reference previously parsed node/way IDs.

When serializing prepared data for another consumer, use `osm::write_osm_xml_string(&data) -> String`. The writer validates that every `<nd ref>` resolves to a known node, so it cannot emit dangling references.

```rust,no_run
use par_osm_rust::osm;
use std::path::Path;

fn write_prepared(data: &osm::OsmData) -> std::io::Result<()> {
    let xml = osm::write_osm_xml_string(data);
    std::fs::write("prepared.osm", xml)
}

fn load_extract(path: &str) -> anyhow::Result<osm::OsmData> {
    // Streams from disk — bounded memory regardless of file size.
    osm::parse_osm_xml_file(path)
}
```

## SRTM elevation

`srtm` resolves and downloads HGT tiles for a bounding box; `elevation::ElevationData` samples elevations from already-downloaded tiles. The two modules are intentionally independent — callers decide whether terrain is required for their workload.

```rust,no_run
use par_osm_rust::{elevation::ElevationData, srtm};

fn main() -> anyhow::Result<()> {
    let bbox = (38.0, -121.0, 38.01, -120.99); // south, west, north, east
    srtm::download_tiles_for_bbox(
        bbox.0,
        bbox.1,
        bbox.2,
        bbox.3,
        &srtm::cache_dir(),
        &|_, _, _| {},
    )?;

    // Load one tile or a directory of .hgt files, then sample bilinearly.
    let elev = ElevationData::from_path(&srtm::cache_dir())?;
    match elev.elevation_at(38.005, -120.995) {
        Some(meters) => println!("ground elevation: {meters} m"),
        None => println!("no tile covers this point or samples are void"),
    }
    Ok(())
}
```

`ElevationData::from_path` accepts a single `.hgt` file or a directory containing multiple `.hgt` files (non-recursive scan); tiles outside the queried region are simply absent from the lookup. `elevation_at(lat, lon)` returns `None` when no tile covers the point or the underlying samples are void.

## Documentation

- [Architecture](docs/ARCHITECTURE.md) - System design, module boundaries, source flow, and cache architecture.
- [Documentation Style Guide](docs/DOCUMENTATION_STYLE_GUIDE.md) - Standards for project documentation and diagrams.

## Release and publishing

Publishing is manual through GitHub Actions. The workflow checks whether the current `Cargo.toml` version already exists on crates.io, runs tests unless explicitly skipped, performs `cargo publish --dry-run`, and then publishes with the `CARGO_REGISTRY_TOKEN` repository secret.

Use the workflow from GitHub Actions:

```text
Actions → Publish to crates.io → Run workflow
```

Before triggering a release, verify locally:

```bash
cargo fmt -- --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo publish --dry-run
```

## Verification

The canonical local gate is `make checkall` — it mirrors what CI runs on every push and pull request:

```bash
make checkall
```

`checkall` (see `Makefile`) runs four steps in order and fails on the first error:

| Step | Target | What it runs |
| --- | --- | --- |
| Format check | `fmt-check` | `cargo fmt -- --check` |
| Lint | `lint` | `cargo clippy --all-targets --all-features -- -D warnings` |
| Type-check | `typecheck` | `cargo check --all-targets` |
| Tests | `test` | `cargo test --all-features` |

The individual targets (`make test`, `make lint`, `make fmt-check`, `make typecheck`) are available when you want to re-run just one step. Note that `make test` runs `cargo test --all-features` — stronger than bare `cargo test`, which would skip `#[cfg(feature = "blocking")]` tests.

CI (`.github/workflows/ci.yml`) runs the `checkall` equivalents across an ubuntu/macos/windows matrix and adds jobs that are impractical to run locally on every save:

- **MSRV check** — `cargo check --all-targets` on toolchain `1.87.0` (the declared MSRV in `Cargo.toml`).
- **Docs** — `cargo doc --no-deps --all-features` with `RUSTDOCFLAGS=-D warnings`.
- **Security audit** — `cargo audit` against a regenerated `Cargo.lock`.
- **Docs lint** — markdownlint-cli2 over `README.md` and `docs/`, plus lychee link-checking the same paths.
