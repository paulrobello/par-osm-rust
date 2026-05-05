//! Shared OpenStreetMap and SRTM fetch, parse, and cache utilities.
//!
//! This crate is shared by `osm-to-bedrock` and `osm-world` through local path
//! dependencies while the API stabilizes. It owns data-source concerns only:
//! OSM parsing, Overpass fetching, raw cache management, SRTM tile downloads,
//! and HGT elevation lookup. It intentionally does not depend on Minecraft,
//! WGPU, UI frameworks, or renderer types.

pub mod cache;
pub mod elevation;
pub mod filter;
pub mod osm;
pub mod osm_cache;
pub mod overpass;
pub mod overture;
pub mod srtm;
