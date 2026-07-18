//! Shared OpenStreetMap-compatible fetch, cache, parse, and normalization utilities.
//!
//! `par-osm-rust` is the data-source crate used by `osm-to-bedrock` and
//! `osm-world`. It owns network and cache concerns only: OSM/Overpass fetching,
//! optional Overture Maps fetching, source merge policy, raw cache management,
//! OSM XML/PBF parsing, SRTM tile downloads, and HGT elevation lookup. It
//! intentionally does not depend on Minecraft, WGPU, UI frameworks, renderer
//! types, or application UI state.
//!
//! # High-level source orchestration
//!
//! Use [`sources::fetch_map_data`] when an application wants one shared path for
//! OSM/Overpass plus optional Overture Maps data:
//!
//! ```no_run
//! # #[cfg(feature = "blocking")] fn main() -> anyhow::Result<()> {
//! use par_osm_rust::filter::FeatureFilter;
//! use par_osm_rust::overture::{OvertureParams, OvertureTheme};
//! use par_osm_rust::sources::{
//!     fetch_map_data, OvertureFailureMode, PoiSourceMode, SourceOptions,
//! };
//!
//! let bbox = (38.0, -121.0, 38.01, -120.99); // south, west, north, east
//! let options = SourceOptions {
//!     filter: FeatureFilter::default(),
//!     overpass_url: None,
//!     use_overpass_cache: true,
//!     overture: OvertureParams {
//!         enabled: true,
//!         themes: vec![OvertureTheme::Place],
//!         ..OvertureParams::default()
//!     },
//!     poi_source_mode: PoiSourceMode::OverturePreferred,
//!     overture_failure_mode: OvertureFailureMode::FallbackToOsm,
//! };
//! let mut progress = |_: f32, _: &str| {};
//! let result = fetch_map_data(bbox, &options, &mut progress)?;
//! println!("source status: {:?}", result.status);
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "blocking"))] fn main() {}
//! ```
//!
//! Important: [`sources::PoiSourceMode::OverturePreferred`] is the default POI
//! policy, but Overture is fetched only when [`overture::OvertureParams::enabled`]
//! is `true`. Default [`sources::SourceOptions`] performs an OSM/Overpass fetch
//! only.
//!
//! # Lower-level modules
//!
//! - [`overpass`] builds safe Overpass QL queries and fetches raw OSM XML.
//! - [`osm_cache`] stores URL-aware raw Overpass XML cache entries.
//! - [`overture`] invokes the optional `overturemaps` CLI and normalizes GeoJSON.
//! - [`source_options`] parses CLI/config strings into source-selection enums.
//! - [`sources`] merges OSM and Overture data with POI source policy and fallback.
//! - [`osm`] parses PBF/XML and writes normalized OSM XML.
//! - [`srtm`] and [`elevation`] download/read HGT elevation data.
//! - [`cache`] resolves shared cache directories and migrates legacy caches.
//! - [`cache_store`] provides the generic raw-payload disk cache ([`cache_store::RawCache`]) shared by [`osm_cache`] and [`overture`].

#![doc(html_root_url = "https://docs.rs/par-osm-rust/0.2.0")]
// DOC-007: every public item in the crate carries a doc comment. Adding
// `missing_docs` turns silent drift into a clippy error under
// `cargo clippy -- -D warnings`, so the gate stays green by construction.
#![warn(missing_docs)]

pub mod cache;
pub mod cache_store;
pub mod elevation;
pub mod filter;
pub mod osm;
pub mod osm_cache;
#[cfg(feature = "blocking")]
pub mod overpass;
pub mod overture;
pub mod source_options;
pub mod sources;
#[cfg(feature = "blocking")]
pub mod srtm;
pub mod synthetic_ids;
