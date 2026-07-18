//! Shared source orchestration for OSM/Overpass plus optional Overture Maps data.
//!
//! This module is the preferred entry point for applications that want a single
//! fetch path with consistent source policy, POI dedupe, fallback warnings, and
//! progress reporting. It always fetches OSM/Overpass data first. Overture is
//! fetched only when [`SourceOptions::overture`] has `enabled = true`; source
//! mode alone never forces an Overture network/CLI request.
//!
//! The pure merge function [`merge_source_data`] is separated from the
//! side-effecting [`fetch_map_data`] entry point so tests and consumers can reuse
//! the policy logic with already-loaded data.

use std::collections::HashMap;

use anyhow::Result;

use crate::filter::FeatureFilter;
use crate::osm::{FeatureSource, OsmData, OsmPoiNode};
use crate::overture::OvertureParams;

/// Policy for which POI source should appear in the normalized output.
///
/// Non-POI Overture geometry may still be merged according to Overture theme
/// priority when Overture data is fetched. This enum only controls the final
/// `OsmData::poi_nodes` collection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoiSourceMode {
    /// Use OSM POIs only.
    ///
    /// OSM/Overpass data is always fetched. When Overture is also enabled via
    /// [`SourceOptions::overture`], non-POI Overture geometry (e.g. building
    /// footprints) is still merged into the result according to Overture theme
    /// priority, but the final `poi_nodes` collection is reset to the OSM POIs
    /// only — any Overture POIs are discarded before the merged result is
    /// returned. Use this mode when you want Overture's richer geometry but
    /// explicitly do not want its POI corpus, or when running against an
    /// Overture release whose POI schema you do not trust.
    OsmOnly,
    /// Use Overture POIs only; OSM POIs are cleared when Overture is unavailable.
    OvertureOnly,
    /// Merge OSM and Overture POIs, deduping near duplicates and preferring Overture
    /// representatives for duplicate groups.
    Both,
    /// Prefer Overture POIs, with OSM POIs as fallback when Overture is missing or
    /// returns no POIs.
    #[default]
    OverturePreferred,
}

/// How [`fetch_map_data`] handles Overture fetch failures when Overture is enabled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OvertureFailureMode {
    /// Return OSM data with a warning when Overture fails.
    #[default]
    FallbackToOsm,
    /// Return an error when Overture fails.
    Fail,
}

/// Configuration for [`fetch_map_data`].
#[derive(Debug, Clone)]
pub struct SourceOptions {
    /// Feature categories to request from OSM/Overpass.
    pub filter: FeatureFilter,
    /// Explicit Overpass endpoint. `None` uses [`crate::overpass::default_overpass_url`].
    pub overpass_url: Option<String>,
    /// Whether to read existing raw Overpass cache entries before fetching.
    /// Freshly fetched Overpass XML is still written to cache on success.
    pub use_overpass_cache: bool,
    /// Overture Maps fetch configuration. Overture is skipped unless `enabled` is `true`.
    pub overture: OvertureParams,
    /// Policy for final POI source selection and dedupe.
    pub poi_source_mode: PoiSourceMode,
    /// Failure policy for Overture fetch errors.
    pub overture_failure_mode: OvertureFailureMode,
}

impl Default for SourceOptions {
    fn default() -> Self {
        Self {
            filter: FeatureFilter::default(),
            overpass_url: None,
            use_overpass_cache: true,
            overture: OvertureParams::default(),
            poi_source_mode: PoiSourceMode::OverturePreferred,
            overture_failure_mode: OvertureFailureMode::FallbackToOsm,
        }
    }
}

/// Effective source outcome after fetching and merging.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceStatus {
    /// Output contains OSM POIs only.
    OsmOnly,
    /// Output contains Overture POIs only.
    OvertureOnly,
    /// Output merged both sources with dedupe.
    Both,
    /// Output preferred Overture POIs successfully.
    OverturePreferred,
    /// Overture was requested but unavailable, failed, or returned no POIs; OSM POIs were used.
    OvertureFallbackToOsm,
}

/// Data and metadata returned by [`fetch_map_data`] and [`merge_source_data`].
pub struct SourceFetchResult {
    /// Normalized map data after source merge policy has been applied.
    pub data: OsmData,
    /// Effective source outcome.
    pub status: SourceStatus,
    /// Human-readable non-fatal warnings, usually Overture fallback reasons.
    pub warnings: Vec<String>,
}

/// Borrowed view of a POI's name tag, or `None` when absent or whitespace-only.
///
/// Returns the raw (untrimmed) string borrowed from `tags`; callers must compare
/// via [`trimmed_lower_eq`] to apply the same trim+lowercase normalization the
/// previous allocating helper produced. A missing or whitespace-only name yields
/// `None`, matching the original semantics so a POI with `name = "   "` is
/// treated the same as a POI with no name tag at all.
fn name_raw(tags: &HashMap<String, String>) -> Option<&str> {
    tags.get("name")
        .map(String::as_str)
        .filter(|name| !name.trim().is_empty())
}

/// Borrowed category key/value pair, or `None` when no POI category tag is present.
///
/// Returns the first matching tag among `amenity`, `shop`, `tourism`, `leisure`,
/// `historic`, `man_made` as `(&'static str, &str)` borrowed from `tags`, so dedupe
/// comparisons allocate zero strings. Two POIs whose tags both miss every category
/// key both return `None` and therefore compare equal — matching the original
/// `"unknown" == "unknown"` behaviour without allocating the sentinel string.
fn poi_category(tags: &HashMap<String, String>) -> Option<(&'static str, &str)> {
    for key in [
        "amenity", "shop", "tourism", "leisure", "historic", "man_made",
    ] {
        if let Some(value) = tags.get(key) {
            return Some((key, value.as_str()));
        }
    }
    None
}

/// Trim+lowercase equality without allocating a normalized `String`.
///
/// Streams each character's `char::to_lowercase` expansion through `flat_map`
/// so the hot dedupe comparison path allocates zero strings. Equivalent to
/// `a.trim().to_lowercase() == b.trim().to_lowercase()` for any UTF-8 input,
/// including ASCII case folding and Unicode expansion (e.g. German `ß`).
fn trimmed_lower_eq(a: &str, b: &str) -> bool {
    a.trim()
        .chars()
        .flat_map(char::to_lowercase)
        .eq(b.trim().chars().flat_map(char::to_lowercase))
}

fn metres_between(a: &OsmPoiNode, b: &OsmPoiNode) -> f64 {
    let mean_lat = ((a.lat + b.lat) * 0.5).to_radians();
    let metres_per_degree_lat = 111_320.0;
    let metres_per_degree_lon = 111_320.0 * mean_lat.cos().abs().max(0.01);
    let dx = (a.lon - b.lon) * metres_per_degree_lon;
    let dz = (a.lat - b.lat) * metres_per_degree_lat;
    (dx * dx + dz * dz).sqrt()
}

fn poi_duplicates(a: &OsmPoiNode, b: &OsmPoiNode) -> bool {
    if poi_category(&a.tags) != poi_category(&b.tags) {
        return false;
    }
    match (name_raw(&a.tags), name_raw(&b.tags)) {
        (Some(a_name), Some(b_name)) if trimmed_lower_eq(a_name, b_name) => {
            metres_between(a, b) <= 25.0
        }
        (None, None) => metres_between(a, b) <= 10.0,
        _ => false,
    }
}

/// Cell size (in metres) for the spatial bucket used by POI dedupe.
///
/// Set to the duplicate distance threshold so the 3×3 neighbor window of any
/// cell is guaranteed to contain every kept POI within duplicate range of the
/// candidate. Each cell axis spans at most this many metres for any latitude.
const DEDUP_CELL_SIZE_M: f64 = 25.0;

/// Snap a lat/lon to an integer grid key whose cells are approximately
/// `DEDUP_CELL_SIZE_M` metres on a side.
///
/// The longitude cell width uses `cos(lat)` so cells stay approximately square
/// in metres across latitudes. Because the cell size equals the duplicate
/// distance threshold (25 m), a 3×3 window around the candidate's cell —
/// `(clat-1..=clat+1, clon-1..=clon+1)` — is guaranteed to contain every kept
/// POI within `25 m`, which is the maximum distance at which two POIs can be
/// considered duplicates (named pairs: 25 m; unnamed pairs: 10 m).
fn dedup_cell(lat: f64, lon: f64) -> (i64, i64) {
    const METRES_PER_DEGREE_LAT: f64 = 111_320.0;
    let metres_per_degree_lon = METRES_PER_DEGREE_LAT * lat.to_radians().cos().abs().max(0.01);
    let cell_lat = (lat * METRES_PER_DEGREE_LAT / DEDUP_CELL_SIZE_M).floor() as i64;
    let cell_lon = (lon * metres_per_degree_lon / DEDUP_CELL_SIZE_M).floor() as i64;
    (cell_lat, cell_lon)
}

fn dedupe_pois_with_overture_preference(mut pois: Vec<OsmPoiNode>) -> Vec<OsmPoiNode> {
    // Sort by source priority so Overture POIs are inserted first; `sort_by_key`
    // is stable, preserving input order within each source group. The first POI
    // of a duplicate group to be inserted is the one kept, so Overture
    // representatives win over OSM/Synthetic duplicates — the same observable
    // preference as the original nested-loop implementation.
    pois.sort_by_key(|poi| match poi.source {
        FeatureSource::Overture => 0,
        FeatureSource::Osm => 1,
        FeatureSource::Synthetic => 2,
    });

    // Spatial bucketing: index each kept POI by its `dedup_cell` key. For each
    // candidate, compare only against kept POIs whose key lies in the
    // candidate's own cell or one of the 8 neighboring cells (3×3 window).
    // Because the cell size equals the duplicate distance threshold, that
    // window is provably sufficient to find every duplicate pair (see
    // [`dedup_cell`]). The per-candidate comparison count is bounded by the
    // cell occupancy, so the algorithm is O(n·k) for uniformly-distributed
    // data instead of the previous O(n²) nested loop.
    let mut cells: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    let mut kept: Vec<OsmPoiNode> = Vec::new();

    for poi in pois {
        let (clat, clon) = dedup_cell(poi.lat, poi.lon);
        let mut duplicate = false;
        'neighborhood: for dlat in -1..=1_i64 {
            for dlon in -1..=1_i64 {
                if let Some(indices) = cells.get(&(clat + dlat, clon + dlon)) {
                    for &idx in indices {
                        if poi_duplicates(&kept[idx], &poi) {
                            duplicate = true;
                            break 'neighborhood;
                        }
                    }
                }
            }
        }
        if !duplicate {
            cells.entry((clat, clon)).or_default().push(kept.len());
            kept.push(poi);
        }
    }
    kept
}

/// Merge already-loaded OSM and optional Overture data according to `poi_source_mode`.
///
/// Duplicate POIs are detected by category, normalized name, and distance. When
/// both sources describe the same POI, the Overture representative is retained.
/// This function performs no network or cache I/O.
///
/// # Examples
///
/// With no Overture data supplied, [`PoiSourceMode::OverturePreferred`] falls
/// back to OSM POIs and reports the fallback status:
///
/// ```
/// use std::collections::HashMap;
///
/// use par_osm_rust::osm::{FeatureSource, OsmData, OsmPoiNode};
/// use par_osm_rust::sources::{merge_source_data, PoiSourceMode, SourceStatus};
///
/// fn empty_osm_data() -> OsmData {
///     OsmData::new(
///         HashMap::new(),
///         Vec::new(),
///         Vec::new(),
///         None,
///         Vec::new(),
///         Vec::new(),
///         Vec::new(),
///     )
/// }
///
/// let mut osm = empty_osm_data();
/// osm.poi_nodes.push(OsmPoiNode {
///     lat: 51.5,
///     lon: -0.1,
///     tags: HashMap::from([
///         ("amenity".to_string(), "restaurant".to_string()),
///         ("name".to_string(), "Diner".to_string()),
///     ]),
///     source: FeatureSource::Osm,
/// });
///
/// let result = merge_source_data(osm, None, PoiSourceMode::OverturePreferred);
/// assert_eq!(result.status, SourceStatus::OvertureFallbackToOsm);
/// assert_eq!(result.data.poi_nodes.len(), 1);
/// ```
pub fn merge_source_data(
    mut osm_data: OsmData,
    overture_data: Option<OsmData>,
    poi_source_mode: PoiSourceMode,
) -> SourceFetchResult {
    let original_osm_pois = osm_data.poi_nodes.clone();
    let mut warnings = Vec::new();

    match (poi_source_mode, overture_data) {
        (PoiSourceMode::OsmOnly, Some(mut overture)) => {
            overture.poi_nodes.clear();
            osm_data.merge(overture);
            osm_data.poi_nodes = original_osm_pois;
            SourceFetchResult {
                data: osm_data,
                status: SourceStatus::OsmOnly,
                warnings,
            }
        }
        (PoiSourceMode::OsmOnly, None) => SourceFetchResult {
            data: osm_data,
            status: SourceStatus::OsmOnly,
            warnings,
        },
        (PoiSourceMode::OvertureOnly, Some(mut overture)) => {
            let overture_pois = overture.poi_nodes.clone();
            osm_data.poi_nodes = overture_pois;
            overture.poi_nodes.clear();
            osm_data.merge(overture);
            SourceFetchResult {
                data: osm_data,
                status: SourceStatus::OvertureOnly,
                warnings,
            }
        }
        (PoiSourceMode::OvertureOnly, None) => {
            osm_data.poi_nodes.clear();
            warnings.push("Overture POIs unavailable for overture-only mode".to_string());
            SourceFetchResult {
                data: osm_data,
                status: SourceStatus::OvertureOnly,
                warnings,
            }
        }
        (PoiSourceMode::Both, Some(mut overture)) => {
            let mut all_pois = original_osm_pois;
            all_pois.extend(overture.poi_nodes.clone());
            overture.poi_nodes.clear();
            osm_data.merge(overture);
            osm_data.poi_nodes = dedupe_pois_with_overture_preference(all_pois);
            SourceFetchResult {
                data: osm_data,
                status: SourceStatus::Both,
                warnings,
            }
        }
        (PoiSourceMode::Both, None) => {
            warnings.push("Overture POIs unavailable; using OSM POIs only".to_string());
            SourceFetchResult {
                data: osm_data,
                status: SourceStatus::OvertureFallbackToOsm,
                warnings,
            }
        }
        (PoiSourceMode::OverturePreferred, Some(mut overture))
            if !overture.poi_nodes.is_empty() =>
        {
            let mut all_pois = original_osm_pois;
            all_pois.extend(overture.poi_nodes.clone());
            overture.poi_nodes.clear();
            osm_data.merge(overture);
            osm_data.poi_nodes = dedupe_pois_with_overture_preference(all_pois);
            SourceFetchResult {
                data: osm_data,
                status: SourceStatus::OverturePreferred,
                warnings,
            }
        }
        (PoiSourceMode::OverturePreferred, Some(mut overture)) => {
            warnings.push("Overture returned no POIs; using OSM POIs only".to_string());
            overture.poi_nodes.clear();
            osm_data.merge(overture);
            osm_data.poi_nodes = original_osm_pois;
            SourceFetchResult {
                data: osm_data,
                status: SourceStatus::OvertureFallbackToOsm,
                warnings,
            }
        }
        (PoiSourceMode::OverturePreferred, None) => {
            warnings.push("Overture POIs unavailable; using OSM POIs only".to_string());
            SourceFetchResult {
                data: osm_data,
                status: SourceStatus::OvertureFallbackToOsm,
                warnings,
            }
        }
    }
}

fn emit_progress(
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

pub(crate) fn fetch_map_data_with_fetchers<FetchOsm, FetchOverture>(
    bbox: (f64, f64, f64, f64),
    options: &SourceOptions,
    progress_cb: &mut dyn FnMut(f32, &str),
    mut fetch_osm: FetchOsm,
    mut fetch_overture: FetchOverture,
) -> Result<SourceFetchResult>
where
    FetchOsm: FnMut((f64, f64, f64, f64), &FeatureFilter, bool, &str) -> Result<OsmData>,
    FetchOverture:
        FnMut((f64, f64, f64, f64), &OvertureParams, &mut dyn FnMut(f32, &str)) -> Result<OsmData>,
{
    const OSM_DONE_PROGRESS: f32 = 0.45;
    const OVERTURE_DONE_PROGRESS: f32 = 0.90;
    const MERGE_PROGRESS: f32 = 0.95;

    let mut last_progress = 0.0;
    emit_progress(progress_cb, &mut last_progress, 0.0, "Fetching OSM data…");
    let default_url = crate::overpass::default_overpass_url();
    let overpass_url = match options.overpass_url.as_deref() {
        Some(url) => url,
        None => &default_url,
    };
    let osm_data = fetch_osm(
        bbox,
        &options.filter,
        options.use_overpass_cache,
        overpass_url,
    )?;

    let overture_data = if options.overture.enabled {
        emit_progress(
            progress_cb,
            &mut last_progress,
            OSM_DONE_PROGRESS,
            "OSM data ready; fetching Overture data…",
        );
        let overture_params = options.overture.clone();
        let mut overture_progress = |pct: f32, message: &str| {
            let pct = if pct.is_finite() {
                pct.clamp(0.0, 1.0)
            } else {
                0.0
            };
            let mapped = OSM_DONE_PROGRESS + pct * (OVERTURE_DONE_PROGRESS - OSM_DONE_PROGRESS);
            emit_progress(progress_cb, &mut last_progress, mapped, message);
        };
        match fetch_overture(bbox, &overture_params, &mut overture_progress) {
            Ok(data) => Some(data),
            Err(err) if options.overture_failure_mode == OvertureFailureMode::FallbackToOsm => {
                let warning = format!("Overture fetch failed: {err:#}");
                log::warn!(
                    "{warning}; continuing with configured POI source mode {:?}",
                    options.poi_source_mode
                );
                let mut result = merge_source_data(osm_data, None, options.poi_source_mode);
                result.warnings.push(warning);
                emit_progress(
                    progress_cb,
                    &mut last_progress,
                    MERGE_PROGRESS,
                    "Merging map data…",
                );
                result.data.clip_to_bbox(bbox);
                emit_progress(progress_cb, &mut last_progress, 1.0, "Map data ready");
                return Ok(result);
            }
            Err(err) => return Err(err),
        }
    } else {
        emit_progress(
            progress_cb,
            &mut last_progress,
            OVERTURE_DONE_PROGRESS,
            "OSM data ready",
        );
        None
    };

    emit_progress(
        progress_cb,
        &mut last_progress,
        MERGE_PROGRESS,
        "Merging map data…",
    );
    let mut result = merge_source_data(osm_data, overture_data, options.poi_source_mode);
    result.data.clip_to_bbox(bbox);
    emit_progress(progress_cb, &mut last_progress, 1.0, "Map data ready");
    Ok(result)
}

/// Fetch OSM/Overpass data, optionally fetch Overture data, and apply source policy.
///
/// `bbox` is `(south, west, north, east)` in decimal degrees. `progress` receives
/// monotonically increasing values in the range `0.0..=1.0` for the source fetch
/// phase. The function uses blocking I/O and should be called from an appropriate
/// worker thread in async/UI applications.
///
/// Overture fetches are gated by `options.overture.enabled`. If Overture is
/// disabled, no Overture CLI check, cache read, or network request is performed
/// even when `options.poi_source_mode` is [`PoiSourceMode::OverturePreferred`].
///
/// # Examples
///
/// ```no_run
/// use par_osm_rust::sources::{fetch_map_data, SourceOptions};
///
/// let bbox = (38.0, -121.0, 38.01, -120.99); // south, west, north, east
/// let options = SourceOptions::default();
/// let mut progress = |pct: f32, msg: &str| println!("{pct:.0}% {msg}");
/// let result = fetch_map_data(bbox, &options, &mut progress).expect("fetch succeeds");
/// println!("status: {:?}", result.status);
/// ```
pub fn fetch_map_data(
    bbox: (f64, f64, f64, f64),
    options: &SourceOptions,
    progress_cb: &mut dyn FnMut(f32, &str),
) -> Result<SourceFetchResult> {
    fetch_map_data_with_fetchers(
        bbox,
        options,
        progress_cb,
        crate::overpass::fetch_osm_data,
        crate::overture::fetch_overture_data,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_options_default_uses_overture_preferred_with_fallback() {
        let options = SourceOptions::default();

        assert_eq!(options.poi_source_mode, PoiSourceMode::OverturePreferred);
        assert_eq!(
            options.overture_failure_mode,
            OvertureFailureMode::FallbackToOsm
        );
        assert!(options.use_overpass_cache);
    }

    fn empty_data() -> OsmData {
        OsmData::new(
            HashMap::new(),
            Vec::new(),
            Vec::new(),
            Some((0.0, 0.0, 1.0, 1.0)),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    }

    fn poi(
        lat: f64,
        lon: f64,
        key: &str,
        value: &str,
        name: &str,
        source: FeatureSource,
    ) -> OsmPoiNode {
        let mut tags = HashMap::from([(key.to_string(), value.to_string())]);
        if !name.is_empty() {
            tags.insert("name".to_string(), name.to_string());
        }
        OsmPoiNode {
            lat,
            lon,
            tags,
            source,
        }
    }

    fn test_bbox() -> (f64, f64, f64, f64) {
        (0.0, 0.0, 1.0, 1.0)
    }

    #[test]
    fn fetch_map_data_default_options_do_not_invoke_overture_fetcher() {
        let options = SourceOptions::default();
        let mut overture_called = false;
        let mut progress = Vec::new();

        let result = fetch_map_data_with_fetchers(
            test_bbox(),
            &options,
            &mut |pct, message| progress.push((pct, message.to_string())),
            |_, _, _, _| {
                let mut osm = empty_data();
                osm.poi_nodes.push(poi(
                    0.5,
                    0.5,
                    "shop",
                    "bakery",
                    "Bakery",
                    FeatureSource::Osm,
                ));
                Ok(osm)
            },
            |_, _, _| {
                overture_called = true;
                panic!("Overture fetcher should not be called when disabled");
            },
        )
        .expect("fetch succeeds");

        assert!(!overture_called);
        assert_eq!(result.status, SourceStatus::OvertureFallbackToOsm);
        assert_eq!(result.data.poi_nodes.len(), 1);
        assert_eq!(result.data.poi_nodes[0].source, FeatureSource::Osm);
        assert_eq!(progress.last().map(|(pct, _)| *pct), Some(1.0));
    }

    #[test]
    fn fetch_map_data_enabled_overture_invokes_fetcher_and_dedupes_preferred_pois() {
        let mut options = SourceOptions::default();
        options.overture.enabled = true;
        options.poi_source_mode = PoiSourceMode::OverturePreferred;
        let mut overture_called = false;

        let result = fetch_map_data_with_fetchers(
            test_bbox(),
            &options,
            &mut |_, _| {},
            |_, _, _, _| {
                let mut osm = empty_data();
                osm.poi_nodes.push(poi(
                    0.50000,
                    0.50000,
                    "amenity",
                    "restaurant",
                    "Diner",
                    FeatureSource::Osm,
                ));
                Ok(osm)
            },
            |_, params, progress| {
                overture_called = true;
                assert!(params.enabled);
                progress(0.0, "Overture starting");
                progress(1.0, "Overture done");
                let mut overture = empty_data();
                overture.poi_nodes.push(poi(
                    0.50005,
                    0.50005,
                    "amenity",
                    "restaurant",
                    "Diner",
                    FeatureSource::Overture,
                ));
                Ok(overture)
            },
        )
        .expect("fetch succeeds");

        assert!(overture_called);
        assert_eq!(result.status, SourceStatus::OverturePreferred);
        assert_eq!(result.data.poi_nodes.len(), 1);
        assert_eq!(result.data.poi_nodes[0].source, FeatureSource::Overture);
    }

    #[test]
    fn fetch_map_data_fallback_captures_overture_error_warning_and_keeps_osm_result() {
        let mut options = SourceOptions::default();
        options.overture.enabled = true;
        options.poi_source_mode = PoiSourceMode::OverturePreferred;
        options.overture_failure_mode = OvertureFailureMode::FallbackToOsm;

        let result = fetch_map_data_with_fetchers(
            test_bbox(),
            &options,
            &mut |_, _| {},
            |_, _, _, _| {
                let mut osm = empty_data();
                osm.poi_nodes.push(poi(
                    0.5,
                    0.5,
                    "shop",
                    "bakery",
                    "Bakery",
                    FeatureSource::Osm,
                ));
                Ok(osm)
            },
            |_, _, _| anyhow::bail!("synthetic overture failure"),
        )
        .expect("fallback succeeds");

        assert_eq!(result.status, SourceStatus::OvertureFallbackToOsm);
        assert_eq!(result.data.poi_nodes.len(), 1);
        assert_eq!(result.data.poi_nodes[0].source, FeatureSource::Osm);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("synthetic overture failure"))
        );
    }

    #[test]
    fn fetch_map_data_strict_overture_failure_returns_error() {
        let mut options = SourceOptions::default();
        options.overture.enabled = true;
        options.overture_failure_mode = OvertureFailureMode::Fail;

        let err = match fetch_map_data_with_fetchers(
            test_bbox(),
            &options,
            &mut |_, _| {},
            |_, _, _, _| Ok(empty_data()),
            |_, _, _| anyhow::bail!("strict overture failure"),
        ) {
            Ok(_) => panic!("strict mode should return Overture error"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("strict overture failure"));
    }

    #[test]
    fn fetch_map_data_progress_is_monotonic_and_finishes_at_one() {
        let mut options = SourceOptions::default();
        options.overture.enabled = true;
        let mut progress_values = Vec::new();

        fetch_map_data_with_fetchers(
            test_bbox(),
            &options,
            &mut |pct, _| progress_values.push(pct),
            |_, _, _, _| Ok(empty_data()),
            |_, _, progress| {
                progress(0.0, "Overture reset to zero");
                progress(0.5, "Overture halfway");
                progress(1.0, "Overture complete");
                Ok(empty_data())
            },
        )
        .expect("fetch succeeds");

        assert!(!progress_values.is_empty());
        for window in progress_values.windows(2) {
            assert!(
                window[0] <= window[1],
                "progress moved backwards: {progress_values:?}"
            );
        }
        assert!(
            progress_values[..progress_values.len() - 1]
                .iter()
                .all(|pct| *pct < 1.0)
        );
        assert_eq!(progress_values.last().copied(), Some(1.0));
    }

    #[test]
    fn osm_only_keeps_osm_pois_and_reports_osm_only_status() {
        let mut osm = empty_data();
        osm.poi_nodes.push(poi(
            0.0,
            0.0,
            "amenity",
            "restaurant",
            "Diner",
            FeatureSource::Osm,
        ));
        let mut overture = empty_data();
        overture.poi_nodes.push(poi(
            0.0,
            0.0,
            "amenity",
            "restaurant",
            "Diner",
            FeatureSource::Overture,
        ));

        let merged = merge_source_data(osm, Some(overture), PoiSourceMode::OsmOnly);

        assert_eq!(merged.status, SourceStatus::OsmOnly);
        assert_eq!(merged.data.poi_nodes.len(), 1);
        assert_eq!(merged.data.poi_nodes[0].source, FeatureSource::Osm);
    }

    #[test]
    fn overture_only_keeps_overture_pois() {
        let mut osm = empty_data();
        osm.poi_nodes.push(poi(
            0.0,
            0.0,
            "amenity",
            "restaurant",
            "Diner",
            FeatureSource::Osm,
        ));
        let mut overture = empty_data();
        overture.poi_nodes.push(poi(
            0.0,
            0.0,
            "amenity",
            "restaurant",
            "Diner",
            FeatureSource::Overture,
        ));

        let merged = merge_source_data(osm, Some(overture), PoiSourceMode::OvertureOnly);

        assert_eq!(merged.data.poi_nodes.len(), 1);
        assert_eq!(merged.data.poi_nodes[0].source, FeatureSource::Overture);
    }

    #[test]
    fn overture_only_without_overture_clears_osm_pois_and_warns() {
        let mut osm = empty_data();
        osm.poi_nodes.push(poi(
            0.0,
            0.0,
            "shop",
            "bakery",
            "Bakery",
            FeatureSource::Osm,
        ));

        let merged = merge_source_data(osm, None, PoiSourceMode::OvertureOnly);

        assert_eq!(merged.status, SourceStatus::OvertureOnly);
        assert!(merged.data.poi_nodes.is_empty());
        assert_eq!(
            merged.warnings,
            vec!["Overture POIs unavailable for overture-only mode".to_string()]
        );
    }

    #[test]
    fn both_dedupes_duplicate_pois_with_overture_winning_and_reports_both_status() {
        let mut osm = empty_data();
        osm.poi_nodes.push(poi(
            51.50000,
            -0.10000,
            "amenity",
            "restaurant",
            "Diner",
            FeatureSource::Osm,
        ));
        let mut overture = empty_data();
        overture.poi_nodes.push(poi(
            51.50005,
            -0.10005,
            "amenity",
            "restaurant",
            "Diner",
            FeatureSource::Overture,
        ));

        let merged = merge_source_data(osm, Some(overture), PoiSourceMode::Both);

        assert_eq!(merged.status, SourceStatus::Both);
        assert_eq!(merged.data.poi_nodes.len(), 1);
        assert_eq!(merged.data.poi_nodes[0].source, FeatureSource::Overture);
    }

    #[test]
    fn same_name_with_category_mismatch_keeps_both_pois() {
        let mut osm = empty_data();
        osm.poi_nodes.push(poi(
            51.50000,
            -0.10000,
            "amenity",
            "restaurant",
            "Corner",
            FeatureSource::Osm,
        ));
        let mut overture = empty_data();
        overture.poi_nodes.push(poi(
            51.50005,
            -0.10005,
            "shop",
            "bakery",
            "Corner",
            FeatureSource::Overture,
        ));

        let merged = merge_source_data(osm, Some(overture), PoiSourceMode::Both);

        assert_eq!(merged.data.poi_nodes.len(), 2);
        assert!(
            merged
                .data
                .poi_nodes
                .iter()
                .any(|poi| poi.source == FeatureSource::Osm)
        );
        assert!(
            merged
                .data
                .poi_nodes
                .iter()
                .any(|poi| poi.source == FeatureSource::Overture)
        );
    }

    #[test]
    fn overture_preferred_dedupes_named_pois_with_overture_winning_and_reports_success() {
        let mut osm = empty_data();
        osm.poi_nodes.push(poi(
            51.50000,
            -0.10000,
            "amenity",
            "restaurant",
            "Diner",
            FeatureSource::Osm,
        ));
        let mut overture = empty_data();
        overture.poi_nodes.push(poi(
            51.50005,
            -0.10005,
            "amenity",
            "restaurant",
            "Diner",
            FeatureSource::Overture,
        ));

        let merged = merge_source_data(osm, Some(overture), PoiSourceMode::OverturePreferred);

        assert_eq!(merged.status, SourceStatus::OverturePreferred);
        assert_eq!(merged.data.poi_nodes.len(), 1);
        assert_eq!(merged.data.poi_nodes[0].source, FeatureSource::Overture);
    }

    #[test]
    fn overture_preferred_falls_back_when_overture_missing() {
        let mut osm = empty_data();
        osm.poi_nodes.push(poi(
            0.0,
            0.0,
            "shop",
            "bakery",
            "Bakery",
            FeatureSource::Osm,
        ));

        let merged = merge_source_data(osm, None, PoiSourceMode::OverturePreferred);

        assert_eq!(merged.status, SourceStatus::OvertureFallbackToOsm);
        assert_eq!(merged.data.poi_nodes.len(), 1);
        assert_eq!(merged.data.poi_nodes[0].source, FeatureSource::Osm);
        assert!(
            merged
                .warnings
                .iter()
                .any(|warning| warning.contains("Overture POIs unavailable"))
        );
    }

    #[test]
    fn overture_preferred_falls_back_precisely_when_overture_returns_zero_pois() {
        let mut osm = empty_data();
        osm.poi_nodes.push(poi(
            0.0,
            0.0,
            "shop",
            "bakery",
            "Bakery",
            FeatureSource::Osm,
        ));
        let overture = empty_data();

        let merged = merge_source_data(osm, Some(overture), PoiSourceMode::OverturePreferred);

        assert_eq!(merged.status, SourceStatus::OvertureFallbackToOsm);
        assert_eq!(merged.data.poi_nodes.len(), 1);
        assert_eq!(merged.data.poi_nodes[0].source, FeatureSource::Osm);
        assert_eq!(
            merged.warnings,
            vec!["Overture returned no POIs; using OSM POIs only".to_string()]
        );
    }

    #[test]
    fn non_poi_overture_tree_nodes_are_preserved_when_pois_are_filtered() {
        let mut osm = empty_data();
        osm.poi_nodes.push(poi(
            0.0,
            0.0,
            "shop",
            "bakery",
            "Bakery",
            FeatureSource::Osm,
        ));
        let mut overture = empty_data();
        overture.tree_nodes.push(crate::osm::OsmNode {
            lat: 51.5,
            lon: -0.1,
        });

        let merged = merge_source_data(osm, Some(overture), PoiSourceMode::OverturePreferred);

        assert_eq!(merged.status, SourceStatus::OvertureFallbackToOsm);
        assert_eq!(merged.data.poi_nodes.len(), 1);
        assert_eq!(merged.data.tree_nodes.len(), 1);
        assert_eq!(merged.data.tree_nodes[0].lat, 51.5);
        assert_eq!(merged.data.tree_nodes[0].lon, -0.1);
    }

    // --- Spatial-grid dedupe tests (ARC-002 / QA-002) -----------------------
    //
    // These tests exercise the O(n·k) spatial-grid path specifically. They
    // place duplicates in cells that a naive single-cell dedupe would miss
    // (adjacent cells, diagonal neighbors, boundary-straddling pairs) and
    // confirm the 3×3 neighbor window catches every duplicate pair while
    // still rejecting pairs beyond the distance threshold.

    /// One degree of latitude ≈ this many metres; matches `metres_between`.
    const METRES_PER_DEGREE_LAT: f64 = 111_320.0;

    #[test]
    fn spatial_grid_dedupes_duplicates_straddling_a_lat_cell_boundary() {
        // Two POIs whose lat positions fall on opposite sides of a cell
        // boundary (one in cell 0, the other in cell 1) but are within the
        // duplicate distance threshold. A naive single-cell dedupe would miss
        // this; the 3×3 neighbor window must catch it.
        let boundary_lat = 25.0 / METRES_PER_DEGREE_LAT; // one cell north of equator
        let epsilon = 1.0e-7;
        let pois = vec![
            poi(
                boundary_lat - epsilon,
                0.0,
                "amenity",
                "restaurant",
                "Border Bistro",
                FeatureSource::Osm,
            ),
            poi(
                boundary_lat + epsilon,
                0.0,
                "amenity",
                "restaurant",
                "Border Bistro",
                FeatureSource::Osm,
            ),
        ];
        let kept = dedupe_pois_with_overture_preference(pois);
        assert_eq!(kept.len(), 1, "boundary-straddling lat pair must dedupe");
    }

    #[test]
    fn spatial_grid_dedupes_duplicates_straddling_a_lon_cell_boundary() {
        // Same boundary-straddling test, but on the longitude axis at the
        // equator. The 3×3 window must look across the lon-cell boundary.
        let boundary_lon = 25.0 / METRES_PER_DEGREE_LAT; // 25 m east at the equator
        let epsilon = 1.0e-7;
        let pois = vec![
            poi(
                0.0,
                boundary_lon - epsilon,
                "amenity",
                "restaurant",
                "Border Bistro",
                FeatureSource::Osm,
            ),
            poi(
                0.0,
                boundary_lon + epsilon,
                "amenity",
                "restaurant",
                "Border Bistro",
                FeatureSource::Osm,
            ),
        ];
        let kept = dedupe_pois_with_overture_preference(pois);
        assert_eq!(kept.len(), 1, "boundary-straddling lon pair must dedupe");
    }

    #[test]
    fn spatial_grid_dedupes_duplicates_in_diagonally_adjacent_cells() {
        // Two POIs near the shared corner of four cells (delta = +1 cell lat,
        // +1 cell lon). The 3×3 window must catch the diagonal neighbor; a
        // rook-neighbourhood (only N/S/E/W) would miss it.
        let boundary = 25.0 / METRES_PER_DEGREE_LAT;
        let epsilon = 1.0e-7;
        let pois = vec![
            poi(
                boundary - epsilon,
                boundary - epsilon,
                "amenity",
                "restaurant",
                "Corner Cafe",
                FeatureSource::Osm,
            ),
            poi(
                boundary + epsilon,
                boundary + epsilon,
                "amenity",
                "restaurant",
                "Corner Cafe",
                FeatureSource::Osm,
            ),
        ];
        let kept = dedupe_pois_with_overture_preference(pois);
        assert_eq!(kept.len(), 1, "diagonal-adjacent pair must dedupe");
    }

    #[test]
    fn spatial_grid_keeps_non_duplicates_in_adjacent_cells() {
        // Two POIs in adjacent cells but > 25 m apart — the 3×3 window visits
        // them, but `poi_duplicates` must reject on the distance threshold.
        // Guards against the spatial grid being over-eager.
        let pois = vec![
            poi(
                0.0,
                0.0,
                "amenity",
                "restaurant",
                "Origin Cafe",
                FeatureSource::Osm,
            ),
            poi(
                0.0,
                30.0 / METRES_PER_DEGREE_LAT, // 30 m east, beyond 25 m threshold
                "amenity",
                "restaurant",
                "Origin Cafe",
                FeatureSource::Osm,
            ),
        ];
        let kept = dedupe_pois_with_overture_preference(pois);
        assert_eq!(
            kept.len(),
            2,
            "near-but-beyond-threshold pair must both keep"
        );
    }

    #[test]
    fn spatial_grid_dedupes_unnamed_pois_within_ten_metres() {
        // POIs with no name tag dedupe within 10 m (not 25 m) per the original
        // `poi_duplicates` semantics. The spatial grid must preserve this.
        let pois = vec![
            poi(0.0, 0.0, "amenity", "restaurant", "", FeatureSource::Osm),
            poi(
                0.0,
                5.0 / METRES_PER_DEGREE_LAT,
                "amenity",
                "restaurant",
                "",
                FeatureSource::Osm,
            ),
        ];
        let kept = dedupe_pois_with_overture_preference(pois);
        assert_eq!(kept.len(), 1, "unnamed pair within 10 m must dedupe");
    }

    #[test]
    fn spatial_grid_preserves_overture_preference_across_cell_boundary() {
        // An OSM POI and an Overture POI straddling a cell boundary with
        // matching category+name. The Overture representative must win,
        // regardless of input order — confirms the spatial grid preserves
        // the Overture-preference semantics exactly.
        let boundary_lat = 25.0 / METRES_PER_DEGREE_LAT;
        let epsilon = 1.0e-7;
        let overture = poi(
            boundary_lat + epsilon,
            0.0,
            "amenity",
            "restaurant",
            "Border Bistro",
            FeatureSource::Overture,
        );
        let osm = poi(
            boundary_lat - epsilon,
            0.0,
            "amenity",
            "restaurant",
            "Border Bistro",
            FeatureSource::Osm,
        );

        // OSM first in input order — sort must still promote Overture.
        let kept = dedupe_pois_with_overture_preference(vec![osm.clone(), overture.clone()]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].source, FeatureSource::Overture);

        // Overture first in input order — must remain Overture.
        let kept = dedupe_pois_with_overture_preference(vec![overture, osm]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].source, FeatureSource::Overture);
    }

    #[test]
    fn spatial_grid_handles_large_synthetic_poi_set() {
        // Synthetic stress test: 1000 distinct POIs at ~100 m spacing (4 cells
        // apart, so no two are within duplicate range), plus 500 near-
        // duplicates displaced by ~7 m. Confirms the O(n·k) spatial-grid path
        // produces the same dedupe result a naive O(n²) algorithm would:
        // 1000 survivors out of 1500 inputs. Correctness check, not a bench.
        let mut pois = Vec::with_capacity(1500);
        for i in 0..1000_i64 {
            let lat = (i as f64) * 100.0 / METRES_PER_DEGREE_LAT;
            let lon = (i as f64) * 100.0 / METRES_PER_DEGREE_LAT;
            pois.push(poi(
                lat,
                lon,
                "amenity",
                "restaurant",
                &format!("Place {i}"),
                FeatureSource::Osm,
            ));
        }
        for i in 0..500_i64 {
            // ~7 m north-east of `Place i` — within the 25 m threshold.
            let lat = (i as f64) * 100.0 / METRES_PER_DEGREE_LAT + 5.0 / METRES_PER_DEGREE_LAT;
            let lon = (i as f64) * 100.0 / METRES_PER_DEGREE_LAT + 5.0 / METRES_PER_DEGREE_LAT;
            pois.push(poi(
                lat,
                lon,
                "amenity",
                "restaurant",
                &format!("Place {i}"),
                FeatureSource::Osm,
            ));
        }
        let kept = dedupe_pois_with_overture_preference(pois);
        assert_eq!(
            kept.len(),
            1000,
            "each of 500 dups collapses onto its source"
        );
    }
}
