# par-osm-rust

Shared Rust utilities for OpenStreetMap and SRTM data access.

This crate is used by `osm-to-bedrock` and `osm-world` through local path dependencies while the API stabilizes:

```toml
par-osm-rust = { path = "../par-osm-rust" }
```

## Cache locations

Default shared cache directories:

- Overpass XML: `~/.cache/par-osm-rust/overpass`
- SRTM HGT: `~/.cache/par-osm-rust/srtm`

Environment override priority:

- Overpass: `PAR_OSM_OVERPASS_CACHE_DIR`, then `OVERPASS_CACHE_DIR`, then the shared default.
- SRTM: `PAR_OSM_SRTM_CACHE_DIR`, then `SRTM_CACHE_DIR`, then the shared default.
- Overpass endpoint: `OVERPASS_URL`, then `https://overpass-api.de/api/interpreter`.

On first use, the crate can migrate legacy caches from:

- `~/.cache/osm-to-bedrock/overpass`
- `~/.cache/osm-to-bedrock/srtm`

## Verification

```bash
cargo fmt -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
```

## Cache migration API

Consumers can explicitly migrate legacy osm-to-bedrock caches before starting their own work:

```rust
let report = par_osm_rust::cache::migrate_legacy_caches()?;
println!("migrated overpass files: {}", report.overpass.moved_files + report.overpass.copied_files);
println!("migrated srtm files: {}", report.srtm.moved_files + report.srtm.copied_files);
```

The regular `par_osm_rust::osm_cache::cache_dir()` and `par_osm_rust::srtm::cache_dir()` helpers also attempt default-location legacy migration on first use.
