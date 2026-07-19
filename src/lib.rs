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
//! use par_osm_rust::bbox::BBox;
//! use par_osm_rust::filter::FeatureFilter;
//! use par_osm_rust::overture::{OvertureParams, OvertureTheme};
//! use par_osm_rust::sources::{
//!     fetch_map_data, OvertureFailureMode, PoiSourceMode, SourceOptions,
//! };
//!
//! let bbox = BBox::new(38.0, -121.0, 38.01, -120.99)?; // south, west, north, east
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
//! let result = fetch_map_data(&bbox, &options, &mut progress)?;
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

#![doc(html_root_url = "https://docs.rs/par-osm-rust/0.2.1")]
// DOC-007: every public item in the crate carries a doc comment. Adding
// `missing_docs` turns silent drift into a clippy error under
// `cargo clippy -- -D warnings`, so the gate stays green by construction.
#![warn(missing_docs)]

/// Shared progress-callback contract (ARC-108, 0.3.0).
///
/// Every public API that reports progress takes `ProgressFn<'a>` instead of
/// spelling out the `&'a mut dyn FnMut(f32, &str)` shape per call site. The
/// `f32` argument is a fraction in `0.0..=1.0` (clamped by the caller); the
/// `&str` is a human-readable status message.
pub type ProgressFn<'a> = &'a mut dyn FnMut(f32, &str);

/// Clamp a progress fraction to `[0.0, 1.0]` and enforce monotonic increase
/// before forwarding to `progress_cb` (ARC-108).
///
/// Non-finite values (NaN/±∞) fall back to the last reported value so a
/// buggy fraction never moves progress backwards. Equal-or-higher values
/// pass through; lower values are silently dropped (the last reported value
/// is kept).
///
/// `last_progress` is the caller's running monotonic cursor — each call site
/// keeps its own local so the clamp is per-fetch, not process-wide.
#[cfg(feature = "blocking")]
pub(crate) fn emit_progress(
    progress_cb: &mut dyn FnMut(f32, &str),
    last_progress: &mut f32,
    pct: f32,
    message: &str,
) {
    let pct = if pct.is_finite() {
        pct.clamp(0.0, 1.0)
    } else {
        *last_progress
    };
    if pct >= *last_progress {
        *last_progress = pct;
        progress_cb(pct, message);
    }
}

// DOC-011: pull README.md into the doctest suite so the ```rust,no_run
// examples compile under `cargo test --doc --all-features`. Any drift
// between the README snippets and the real API becomes a CI failure instead
// of silent documentation rot. The struct form (vs. a `mod`) needs no
// `missing_docs` waiver: the `#[doc = include_str!(...)]` attribute above
// the struct supplies the doc comment.
//
// Gated on `feature = "blocking"` because every README example drives the
// network surface (Overpass/SRTM/Overture fetchers) which the pure
// `--no-default-features` subset does not compile — the lib.rs doctest
// above uses the same gating. CI's `make checkall` runs with
// `--all-features`, which is where drift detection fires.
#[cfg(all(doctest, feature = "blocking"))]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

// ARC-106: `bbox` is now `pub` and compiles under both `--all-features` and
// `--no-default-features` because the `BBox` newtype is consumed by pure
// modules (`osm_cache`, `overture::cache`) in addition to the blocking-gated
// fetchers. Previously `pub(crate)` and `#[cfg(feature = "blocking")]` (the
// internal helper was used only by blocking modules); the public `BBox`
// boundary type broadens the dependency surface.
pub mod bbox;
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
// QA-107: shared byte-boundary-safe truncation helpers used by `overpass`
// (error-body clipping) and `overture::cli` (stderr clipping). Crate-private
// so the public API is unchanged. Feature-gated to `blocking` because both
// call sites are blocking-only — without the feature the module would
// otherwise emit dead-code warnings.
#[cfg(feature = "blocking")]
pub(crate) mod text_truncate;
