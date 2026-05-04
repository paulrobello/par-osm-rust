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
