//! Overture Maps integration via the `overturemaps` Python CLI.
//!
//! This module provides helpers for checking whether the Overture CLI is
//! installed on the system PATH, invoking it to download GeoJSON data for a
//! given theme and bounding box, and converting the resulting GeoJSON into
//! the `OsmData` structure used by the rest of the pipeline.
//!
//! The `overturemaps` CLI (PyPI: `overturemaps`) is an optional runtime
//! dependency — callers should check [`is_cli_available`] before attempting
//! any download.  If the CLI is absent, the integration is silently skipped.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use crate::cache_store::{CacheMeta as CacheMetaTrait, RawCache};
use crate::osm::{FeatureSource, OsmData, OsmNode, OsmPoiNode, OsmWay};
use crate::synthetic_ids::OvertureIdAllocator;

/// Environment override that selects an absolute path to the `overturemaps`
/// CLI binary, bypassing the default PATH lookup (SEC-010). Must be set to an
/// absolute path that exists on disk; relative paths or missing files fall
/// back to the PATH-based `overturemaps` lookup.
const OVERTURE_CLI_ENV_OVERRIDE: &str = "PAR_OSM_OVERTURE_CLI";

/// Default cache entry TTL when [`OvertureParams::cache_ttl_secs`] is `None`:
/// ~30 days. Entries older than this are treated as misses and re-fetched
/// (ARC-001).
const OVERTURE_CACHE_DEFAULT_TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// Overture Maps theme selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OvertureTheme {
    /// Building footprints and building metadata.
    Building,
    /// Transportation segments, normalized mostly as OSM-style roads.
    Transportation,
    /// Places and POIs, normalized into tagged POI nodes.
    Place,
    /// Base land, land-use, water, and tree features.
    Base,
    /// Address points, normalized into address nodes.
    Address,
}

impl OvertureTheme {
    /// Return all supported themes in a stable default order.
    pub fn all() -> Vec<Self> {
        vec![
            Self::Building,
            Self::Transportation,
            Self::Place,
            Self::Base,
            Self::Address,
        ]
    }

    /// Return the `overturemaps download --type` values used for this theme.
    pub fn cli_types(&self) -> Vec<&'static str> {
        match self {
            Self::Building => vec!["building"],
            Self::Transportation => vec!["segment"],
            Self::Place => vec!["place"],
            Self::Base => vec!["land", "land_use", "water"],
            Self::Address => vec!["address"],
        }
    }

    /// Parse a user-facing theme string, accepting singular/plural aliases.
    ///
    /// # Examples
    ///
    /// ```
    /// use par_osm_rust::overture::OvertureTheme;
    ///
    /// assert_eq!(
    ///     OvertureTheme::from_str_loose("Buildings"),
    ///     Some(OvertureTheme::Building)
    /// );
    /// assert_eq!(
    ///     OvertureTheme::from_str_loose("road"),
    ///     Some(OvertureTheme::Transportation)
    /// );
    /// assert!(OvertureTheme::from_str_loose("unknown").is_none());
    /// ```
    pub fn from_str_loose(s: &str) -> Option<Self> {
        let theme = s.to_lowercase();
        match theme.as_str() {
            "address" | "addresses" | "addr" => Some(Self::Address),
            _ => match theme.strip_suffix('s').unwrap_or(&theme) {
                "building" => Some(Self::Building),
                "transportation" | "transport" | "road" | "segment" => Some(Self::Transportation),
                "place" => Some(Self::Place),
                "base" | "land" | "land_use" | "landuse" | "water" => Some(Self::Base),
                _ => None,
            },
        }
    }
}

impl std::fmt::Display for OvertureTheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Building => write!(f, "building"),
            Self::Transportation => write!(f, "transportation"),
            Self::Place => write!(f, "place"),
            Self::Base => write!(f, "base"),
            Self::Address => write!(f, "address"),
        }
    }
}

/// Which data source wins when Overture and OSM both cover the same non-POI theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePriority {
    /// Prefer Overture features for this theme.
    Overture,
    /// Prefer OSM/Overpass features for this theme.
    Osm,
    /// Keep features from both sources.
    #[default]
    Both,
}

/// Parameters controlling Overture Maps data integration.
///
/// `cache_ttl_secs` controls how long a cached GeoJSON entry is considered
/// fresh; entries older than the TTL are treated as misses and re-fetched
/// (ARC-001). `None` means "use the default ~30-day TTL"; `Some(0)` disables
/// the cache (every fetch hits the CLI). Constructing via [`Default`] yields
/// `None`, equivalent to the documented default.
///
/// # Examples
///
/// ```
/// use par_osm_rust::overture::{OvertureParams, OvertureTheme};
///
/// // Default: Overture disabled, all themes, ~30-day cache TTL.
/// let default = OvertureParams::default();
/// assert!(!default.enabled);
/// assert_eq!(default.themes.len(), OvertureTheme::all().len());
///
/// // Enable just the building theme and shorten the cache TTL to one day.
/// let params = OvertureParams {
///     enabled: true,
///     themes: vec![OvertureTheme::Building],
///     cache_ttl_secs: Some(24 * 60 * 60),
///     ..Default::default()
/// };
/// assert!(params.enabled);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OvertureParams {
    /// Whether Overture should be fetched. Defaults to `false`.
    pub enabled: bool,
    /// Overture themes to fetch when enabled. Defaults to all supported themes.
    pub themes: Vec<OvertureTheme>,
    /// Per-theme source priority for non-POI features. Missing entries default to [`ThemePriority::Both`].
    pub priority: HashMap<OvertureTheme, ThemePriority>,
    /// Timeout for each Overture CLI download command.
    pub timeout_secs: u64,
    /// Maximum age in seconds for a cache entry to be considered fresh
    /// (ARC-001). `None` selects the default ~30-day TTL; `Some(0)` disables
    /// the cache. Stored as seconds so the struct remains `Serialize`/`Deserialize`
    /// without an extra serde helper for `Duration`.
    #[serde(default)]
    pub cache_ttl_secs: Option<u64>,
}

impl Default for OvertureParams {
    fn default() -> Self {
        Self {
            enabled: false,
            themes: OvertureTheme::all(),
            priority: HashMap::new(),
            timeout_secs: 120,
            cache_ttl_secs: None,
        }
    }
}

impl OvertureParams {
    /// Return the configured priority for `theme`, defaulting to [`ThemePriority::Both`].
    pub fn priority_for(&self, theme: OvertureTheme) -> ThemePriority {
        self.priority
            .get(&theme)
            .copied()
            .unwrap_or(ThemePriority::Both)
    }

    /// Resolve the effective cache TTL as a [`Duration`].
    ///
    /// `None` (the default) selects the documented ~30-day TTL. `Some(0)`
    /// disables the cache by yielding a zero-length TTL, which forces every
    /// read to miss. Any other `Some(secs)` is returned verbatim. ARC-001.
    pub fn cache_ttl(&self) -> Duration {
        match self.cache_ttl_secs {
            None => Duration::from_secs(OVERTURE_CACHE_DEFAULT_TTL_SECS),
            Some(secs) => Duration::from_secs(secs),
        }
    }
}

// ── Synthetic node-ID allocation ──────────────────────────────────────────
//
// Overture geometry nodes and ways do not carry OSM IDs, so each parse
// assigns synthetic IDs from a fresh [`OvertureIdAllocator`] owned by
// `parse_overture_geojson`. The allocator starts at
// `SYNTHETIC_OVERTURE_ID_BASE` and decrements per ID, making parses
// deterministic (ARC-009 / QA-010) and keeping the Overture range disjoint
// from the writer's node/way/relation ranges and from real OSM IDs.
// See `crate::synthetic_ids` for the centralized contract.

// ── CLI availability check ────────────────────────────────────────────────

const CLI_CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const CLI_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Resolve the `overturemaps` CLI binary path (SEC-010).
///
/// Resolution order:
/// 1. `PAR_OSM_OVERTURE_CLI` environment override, if it points to an existing
///    absolute path. Absolute paths are exec'd directly, bypassing PATH lookup
///    (which is vulnerable to binary-hijack in multi-user / shared-PATH
///    setups where another user can shadow `overturemaps` earlier on PATH).
/// 2. Fall back to the bare program name `"overturemaps"`, resolved by the
///    operating system via `PATH` (current default behavior).
///
/// Relative paths and paths to non-existent files silently fall back to the
/// PATH lookup so a misconfigured override never blocks a fetch.
fn resolve_overture_cli() -> PathBuf {
    if let Ok(raw) = std::env::var(OVERTURE_CLI_ENV_OVERRIDE) {
        let path = PathBuf::from(&raw);
        if path.is_absolute() && path.exists() {
            return path;
        }
        log::debug!(
            "PAR_OSM_OVERTURE_CLI='{raw}' is not an absolute path to an existing file; falling back to PATH lookup"
        );
    }
    PathBuf::from("overturemaps")
}

/// Check whether the `overturemaps` CLI is available on the system PATH (or
/// via the `PAR_OSM_OVERTURE_CLI` override — see [`resolve_overture_cli`]).
///
/// Runs `overturemaps --version` with a short timeout.  Returns `true` if
/// the command succeeds (exit code 0), `false` otherwise.
pub fn is_cli_available() -> bool {
    let Ok(mut child) = std::process::Command::new(resolve_overture_cli())
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if start.elapsed() >= CLI_CHECK_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(CLI_POLL_INTERVAL);
            }
            Err(_) => return false,
        }
    }
}

/// Best-effort probe of the `overturemaps` CLI version string (ARC-001).
///
/// Spawn-and-poll `overturemaps --version`, capturing stdout. Returns the
/// first non-empty trimmed line of stdout on success. On any failure
/// (spawn error, non-zero exit, timeout, malformed UTF-8), returns
/// `"unknown"` so the caller can still fold *something* into the cache key
/// without blocking the fetch. Cached per-process via [`OnceLock`].
fn probe_cli_version_uncached() -> String {
    let cli = resolve_overture_cli();
    let Ok(mut child) = std::process::Command::new(&cli)
        .arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return "unknown".to_string();
    };

    let start = Instant::now();
    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= CLI_CHECK_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return "unknown".to_string();
                }
                std::thread::sleep(CLI_POLL_INTERVAL);
            }
            Err(_) => return "unknown".to_string(),
        }
    };

    if !exit_status.success() {
        return "unknown".to_string();
    }

    let Some(mut stdout) = child.stdout.take() else {
        return "unknown".to_string();
    };
    let mut buf = String::new();
    if stdout.read_to_string(&mut buf).is_err() {
        return "unknown".to_string();
    }
    buf.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

/// Process-wide cached `overturemaps` CLI version. Probed once on first use
/// (ARC-001); subsequent calls return the cached value without re-shelling
/// out. Probing failure yields `"unknown"`.
fn cached_cli_version() -> &'static str {
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED.get_or_init(probe_cli_version_uncached)
}

const STDERR_SNIPPET_LIMIT: usize = 4096;

fn stderr_suffix(stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        String::new()
    } else if stderr.len() <= STDERR_SNIPPET_LIMIT {
        format!(": {stderr}")
    } else {
        let head_len = STDERR_SNIPPET_LIMIT / 2;
        let tail_len = STDERR_SNIPPET_LIMIT - head_len;
        let head = str_prefix_at_boundary(stderr, head_len);
        let tail = str_suffix_at_boundary(stderr, tail_len);
        let omitted = stderr.len().saturating_sub(head.len() + tail.len());
        format!(": {head}\n...[stderr truncated, {omitted} bytes omitted]...\n{tail}")
    }
}

fn str_prefix_at_boundary(s: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn str_suffix_at_boundary(s: &str, max_bytes: usize) -> &str {
    let mut start = s.len().saturating_sub(max_bytes);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

fn read_stderr_file(stderr_path: &Path, cli_type: &str) -> Result<Vec<u8>> {
    std::fs::read(stderr_path)
        .with_context(|| format!("reading overturemaps stderr for type '{cli_type}'"))
}

fn wait_with_stderr_file_timeout(
    mut child: std::process::Child,
    stderr_path: &Path,
    timeout: Duration,
    timeout_secs: u64,
    cli_type: &str,
) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    let start = Instant::now();
    loop {
        match child.try_wait().context("polling overturemaps CLI")? {
            Some(status) => {
                let stderr = read_stderr_file(stderr_path, cli_type)?;
                return Ok((status, stderr));
            }
            None => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    child
                        .wait()
                        .context("waiting for overturemaps CLI after timeout")?;
                    let stderr = read_stderr_file(stderr_path, cli_type)?;
                    let stderr_msg = stderr_suffix(&stderr);
                    bail!(
                        "overturemaps CLI timed out after {timeout_secs}s for type '{cli_type}'{stderr_msg}"
                    );
                }
                std::thread::sleep(CLI_POLL_INTERVAL);
            }
        }
    }
}

// ── GeoJSON download via CLI ──────────────────────────────────────────────

/// Validate a `cli_type` string before passing it to the `overturemaps` CLI
/// (SEC-012).
///
/// The crate's own use is safe (themes come from [`OvertureTheme::cli_types`]),
/// but [`fetch_geojson_for_type`] is `pub` and an external caller could pass
/// user input. A `cli_type` containing `-` or whitespace enables argument
/// injection against the CLI's arg parser (e.g. `--output=...` would be
/// honored as a flag, not a positional `--type` value). Reject those shapes
/// at the public boundary; the shell-out itself is already arg-vector based
/// (no shell), so this is the only remaining injection vector.
fn validate_cli_type(cli_type: &str) -> Result<()> {
    if cli_type.is_empty() {
        bail!("overturemaps cli_type must not be empty");
    }
    if cli_type.contains('-') || cli_type.chars().any(char::is_whitespace) {
        bail!(
            "overturemaps cli_type '{cli_type}' rejected: contains '-' or whitespace \
             (argument-injection guard, SEC-012)"
        );
    }
    Ok(())
}

/// Download Overture GeoJSON for a single CLI type and bounding box.
///
/// Honors the `PAR_OSM_OVERTURE_CLI` environment override for the binary
/// path (see [`resolve_overture_cli`], SEC-010). The shell-out is
/// arg-vector based (no shell); `cli_type` is validated by
/// [`validate_cli_type`] to reject argument-injection attempts (SEC-012).
///
/// Invokes:
/// ```text
/// overturemaps download --bbox W,S,E,N -t <cli_type> -o <tmpfile>
/// ```
///
/// # Arguments
///
/// * `cli_type` – The Overture type string (e.g. `"building"`, `"segment"`).
///   Must not contain `-` or whitespace.
/// * `bbox` – `(min_lat, min_lon, max_lat, max_lon)` bounding box.
/// * `timeout_secs` – Maximum wall-clock seconds to wait for the CLI.
///
/// # Returns
///
/// The GeoJSON string written by the CLI, or an error if the CLI fails or
/// times out.
pub fn fetch_geojson_for_type(
    cli_type: &str,
    bbox: (f64, f64, f64, f64),
    timeout_secs: u64,
) -> Result<String> {
    validate_cli_type(cli_type)?;

    let (min_lat, min_lon, max_lat, max_lon) = bbox;
    // Overture CLI expects W,S,E,N order (min_lon, min_lat, max_lon, max_lat).
    let bbox_str = format!("{min_lon},{min_lat},{max_lon},{max_lat}");

    // Write output to a named temp file so the CLI can stream to disk.
    let tmp = tempfile::Builder::new()
        .suffix(".geojson")
        .tempfile()
        .context("creating temp file for overturemaps output")?;
    let tmp_path = tmp.path().to_path_buf();

    let stderr_tmp = tempfile::Builder::new()
        .suffix(".stderr")
        .tempfile()
        .context("creating temp file for overturemaps stderr")?;
    let stderr_path = stderr_tmp.path().to_path_buf();
    let stderr_file = stderr_tmp
        .reopen()
        .context("opening temp file for overturemaps stderr")?;

    let cli_path = resolve_overture_cli();
    let child = std::process::Command::new(&cli_path)
        .arg("download")
        .arg("-f")
        .arg("geojson")
        .arg("--bbox")
        .arg(&bbox_str)
        .arg("-t")
        .arg(cli_type)
        .arg("-o")
        .arg(&tmp_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()
        .with_context(|| format!("spawning overturemaps CLI at {}", cli_path.display()))?;

    let (status, stderr) = wait_with_stderr_file_timeout(
        child,
        &stderr_path,
        Duration::from_secs(timeout_secs),
        timeout_secs,
        cli_type,
    )?;

    if !status.success() {
        let stderr_msg = stderr_suffix(&stderr);
        bail!(
            "overturemaps CLI exited with status {} for type '{cli_type}'{stderr_msg}",
            status.code().unwrap_or(-1)
        );
    }

    let content = std::fs::read_to_string(&tmp_path)
        .with_context(|| format!("reading overturemaps output for type '{cli_type}'"))?;

    Ok(content)
}

// ── GeoJSON → OsmData conversion ─────────────────────────────────────────

/// Update a running bounding-box accumulator with a new coordinate.
fn update_bounds(
    min_lat: &mut f64,
    min_lon: &mut f64,
    max_lat: &mut f64,
    max_lon: &mut f64,
    lat: f64,
    lon: f64,
) {
    *min_lat = min_lat.min(lat);
    *min_lon = min_lon.min(lon);
    *max_lat = max_lat.max(lat);
    *max_lon = max_lon.max(lon);
}

/// Convert a GeoJSON coordinate array `[lon, lat]` or `[lon, lat, ele]` to an
/// `(OsmNode, i64)` pair and update the bounding-box accumulator.
///
/// Returns the synthetic node ID (drawn from `id_alloc`) and the node, or
/// `None` if the array is malformed.
fn coord_to_node(
    coord: &Value,
    id_alloc: &mut OvertureIdAllocator,
    min_lat: &mut f64,
    min_lon: &mut f64,
    max_lat: &mut f64,
    max_lon: &mut f64,
) -> Option<(i64, OsmNode)> {
    let arr = coord.as_array()?;
    let lon = arr.first()?.as_f64()?;
    let lat = arr.get(1)?.as_f64()?;
    update_bounds(min_lat, min_lon, max_lat, max_lon, lat, lon);
    Some((id_alloc.next_id(), OsmNode { lat, lon }))
}

/// Convert a GeoJSON coordinate array (ring or line) into a list of node IDs
/// and the corresponding node map entries.
///
/// Each element of `coords` is expected to be a `[lon, lat]` array. IDs are
/// drawn from `id_alloc`.
fn coords_to_nodes(
    coords: &[Value],
    id_alloc: &mut OvertureIdAllocator,
    min_lat: &mut f64,
    min_lon: &mut f64,
    max_lat: &mut f64,
    max_lon: &mut f64,
) -> (Vec<i64>, HashMap<i64, OsmNode>) {
    let mut node_refs = Vec::with_capacity(coords.len());
    let mut nodes = HashMap::with_capacity(coords.len());
    for coord in coords {
        if let Some((id, node)) = coord_to_node(coord, id_alloc, min_lat, min_lon, max_lat, max_lon)
        {
            node_refs.push(id);
            nodes.insert(id, node);
        }
    }
    (node_refs, nodes)
}

/// Build one way from a coordinate ring/line, appending it (and any new nodes)
/// to the running accumulators. Shared by the LineString / Polygon /
/// MultiPolygon branches of [`parse_overture_geojson`] (QA-006).
///
/// Behavior preserved exactly from the prior inlined branches:
/// - If `coords` produces zero valid node refs, nothing is pushed.
/// - Otherwise a synthetic way ID is allocated, the way is appended with
///   `tags` (moved), and the new nodes are merged into `nodes`.
///
/// Argument count exceeds clippy's default threshold because the four
/// bounding-box accumulators are passed individually, mirroring the existing
/// `coord_to_node` / `coords_to_nodes` style. Bundling them into a struct
/// would require touching those helpers too, which is out of scope for this
/// dedupe pass (QA-006).
#[allow(clippy::too_many_arguments)]
fn push_way_from_coords(
    coords: &[Value],
    id_alloc: &mut OvertureIdAllocator,
    nodes: &mut HashMap<i64, OsmNode>,
    ways_with_ids: &mut Vec<(i64, OsmWay)>,
    tags: HashMap<String, String>,
    min_lat: &mut f64,
    min_lon: &mut f64,
    max_lat: &mut f64,
    max_lon: &mut f64,
) {
    let (node_refs, new_nodes) =
        coords_to_nodes(coords, id_alloc, min_lat, min_lon, max_lat, max_lon);
    if node_refs.is_empty() {
        return;
    }
    let way_id = id_alloc.next_id();
    ways_with_ids.push((way_id, OsmWay { tags, node_refs }));
    nodes.extend(new_nodes);
}

/// Map an Overture place category string to the appropriate OSM primary key.
fn map_place_category_to_osm_key(category: &str) -> &'static str {
    match category {
        "restaurant" | "cafe" | "bar" | "fast_food" | "food_and_drink" => "amenity",
        "supermarket" | "grocery" | "clothing" | "electronics" | "retail" => "shop",
        "hotel" | "motel" | "hostel" | "accommodation" => "tourism",
        "park" | "playground" | "sports_centre" | "stadium" | "recreation" => "leisure",
        _ => "amenity",
    }
}

/// Map Overture feature properties to OSM-style tags for the given theme.
fn map_tags_for_theme(props: &Value, theme: OvertureTheme) -> HashMap<String, String> {
    let mut tags: HashMap<String, String> = HashMap::new();

    match theme {
        OvertureTheme::Building => {
            // class → building (default "yes")
            let class = props.get("class").and_then(|v| v.as_str()).unwrap_or("yes");
            tags.insert("building".into(), class.to_string());

            // height → building:height
            if let Some(h) = props.get("height").and_then(|v| v.as_f64()) {
                tags.insert("building:height".into(), h.to_string());
            }
            // num_floors → building:levels
            if let Some(f) = props.get("num_floors").and_then(|v| v.as_u64()) {
                tags.insert("building:levels".into(), f.to_string());
            }
        }

        OvertureTheme::Transportation => {
            // class → highway (default "unclassified")
            let class = props
                .get("class")
                .and_then(|v| v.as_str())
                .unwrap_or("unclassified");
            tags.insert("highway".into(), class.to_string());

            // names.primary → name
            if let Some(name) = props
                .get("names")
                .and_then(|n| n.get("primary"))
                .and_then(|v| v.as_str())
            {
                tags.insert("name".into(), name.to_string());
            }
            // road_surface → surface
            if let Some(surface) = props.get("road_surface").and_then(|v| v.as_str()) {
                tags.insert("surface".into(), surface.to_string());
            }
            // is_bridge → bridge=yes
            if props
                .get("is_bridge")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                tags.insert("bridge".into(), "yes".into());
            }
            // is_tunnel → tunnel=yes
            if props
                .get("is_tunnel")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                tags.insert("tunnel".into(), "yes".into());
            }
        }

        OvertureTheme::Place => {
            // categories.primary → amenity / shop / tourism / leisure
            if let Some(category) = props
                .get("categories")
                .and_then(|c| c.get("primary"))
                .and_then(|v| v.as_str())
            {
                let osm_key = map_place_category_to_osm_key(category);
                tags.insert(osm_key.into(), category.to_string());
            }
            // names.primary → name
            if let Some(name) = props
                .get("names")
                .and_then(|n| n.get("primary"))
                .and_then(|v| v.as_str())
            {
                tags.insert("name".into(), name.to_string());
            }
        }

        OvertureTheme::Base => {
            // Overture Base uses "subtype" and "class" to distinguish features.
            // We map them to the appropriate OSM keys.
            let subtype = props.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
            let class = props.get("class").and_then(|v| v.as_str()).unwrap_or("");

            // Water bodies
            if matches!(
                subtype,
                "water" | "lake" | "pond" | "reservoir" | "ocean" | "sea"
            ) {
                tags.insert("natural".into(), "water".into());
                if !subtype.is_empty() && subtype != "water" {
                    tags.insert("water".into(), subtype.to_string());
                }
            }
            // Waterways
            else if matches!(subtype, "river" | "stream" | "canal" | "drain" | "ditch") {
                tags.insert("waterway".into(), subtype.to_string());
            }
            // Land use — from class when subtype indicates land_use
            else if matches!(
                subtype,
                "forest"
                    | "farmland"
                    | "residential"
                    | "commercial"
                    | "industrial"
                    | "cemetery"
                    | "grass"
                    | "scrub"
                    | "farmyard"
            ) {
                tags.insert("landuse".into(), subtype.to_string());
            }
            // Natural land cover from class
            else if matches!(subtype, "land" | "")
                && matches!(
                    class,
                    "grass" | "scrub" | "heath" | "bare_rock" | "sand" | "beach"
                )
            {
                tags.insert("natural".into(), class.to_string());
            }
            // Leisure areas
            else if matches!(subtype, "park" | "garden" | "pitch" | "playground") {
                tags.insert("leisure".into(), subtype.to_string());
            }
            // Individual tree points
            else if subtype == "tree" {
                tags.insert("natural".into(), "tree".into());
            }
            // Fallback: try the class field
            else if !class.is_empty() {
                tags.insert("landuse".into(), class.to_string());
            }
        }

        OvertureTheme::Address => {
            // number → addr:housenumber
            if let Some(number) = props.get("number").and_then(|v| v.as_str()) {
                tags.insert("addr:housenumber".into(), number.to_string());
            }
            // street → addr:street
            if let Some(street) = props.get("street").and_then(|v| v.as_str()) {
                tags.insert("addr:street".into(), street.to_string());
            }
        }
    }

    tags
}

/// Parse an Overture GeoJSON `FeatureCollection` string into an [`OsmData`].
///
/// Each GeoJSON feature is converted according to `theme`:
///
/// - `Point` geometries become POI nodes (Place theme) or address nodes (Address theme).
/// - `LineString` geometries become ways.
/// - `Polygon` geometries become ways using the outer ring.
/// - `MultiPolygon` geometries produce one way per polygon outer ring.
///
/// Synthetic negative node IDs are assigned to avoid collision with OSM IDs.
pub fn parse_overture_geojson(geojson_str: &str, theme: OvertureTheme) -> Result<OsmData> {
    let root: Value = serde_json::from_str(geojson_str).context("parsing Overture GeoJSON")?;

    let features = root
        .get("features")
        .and_then(|f| f.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[]);

    // Fresh per-parse allocator: identical GeoJSON inputs produce identical
    // synthetic ID sequences (ARC-009 / QA-010). See `crate::synthetic_ids`.
    let mut id_alloc = OvertureIdAllocator::new();

    let mut nodes: HashMap<i64, OsmNode> = HashMap::new();
    let mut ways_with_ids: Vec<(i64, OsmWay)> = Vec::new();
    let mut poi_nodes: Vec<OsmPoiNode> = Vec::new();
    let mut addr_nodes: Vec<OsmPoiNode> = Vec::new();
    let mut tree_nodes: Vec<OsmNode> = Vec::new();

    let mut min_lat = f64::MAX;
    let mut min_lon = f64::MAX;
    let mut max_lat = f64::MIN;
    let mut max_lon = f64::MIN;

    for feature in features {
        let props = feature.get("properties").unwrap_or(&Value::Null);
        let tags = map_tags_for_theme(props, theme);

        let geometry = match feature.get("geometry") {
            Some(g) => g,
            None => continue,
        };
        let geom_type = geometry.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let coordinates = geometry.get("coordinates");

        match geom_type {
            "Point" => {
                if let Some(coord) = coordinates
                    && let Some((id, node)) = coord_to_node(
                        coord,
                        &mut id_alloc,
                        &mut min_lat,
                        &mut min_lon,
                        &mut max_lat,
                        &mut max_lon,
                    )
                {
                    nodes.insert(id, node);
                    let poi = OsmPoiNode {
                        lat: node.lat,
                        lon: node.lon,
                        tags: tags.clone(),
                        source: FeatureSource::Overture,
                    };
                    match theme {
                        OvertureTheme::Address => addr_nodes.push(poi),
                        OvertureTheme::Place => poi_nodes.push(poi),
                        _ => {
                            // Decorative tree nodes from land theme
                            if tags.get("natural").map(|s| s.as_str()) == Some("tree") {
                                tree_nodes.push(OsmNode {
                                    lat: node.lat,
                                    lon: node.lon,
                                });
                            }
                        }
                    }
                }
            }

            "LineString" => {
                if let Some(coords) = coordinates.and_then(|c| c.as_array()) {
                    push_way_from_coords(
                        coords,
                        &mut id_alloc,
                        &mut nodes,
                        &mut ways_with_ids,
                        tags,
                        &mut min_lat,
                        &mut min_lon,
                        &mut max_lat,
                        &mut max_lon,
                    );
                }
            }

            "Polygon" => {
                // Use the outer ring (first element).
                if let Some(outer_ring) = coordinates
                    .and_then(|c| c.as_array())
                    .and_then(|rings| rings.first())
                    .and_then(|r| r.as_array())
                {
                    push_way_from_coords(
                        outer_ring,
                        &mut id_alloc,
                        &mut nodes,
                        &mut ways_with_ids,
                        tags,
                        &mut min_lat,
                        &mut min_lon,
                        &mut max_lat,
                        &mut max_lon,
                    );
                }
            }

            "MultiPolygon" => {
                // Each polygon produces one way from its outer ring.
                if let Some(polygons) = coordinates.and_then(|c| c.as_array()) {
                    for polygon in polygons {
                        if let Some(outer_ring) = polygon
                            .as_array()
                            .and_then(|rings| rings.first())
                            .and_then(|r| r.as_array())
                        {
                            // `tags` is cloned per polygon so each ring gets
                            // its own copy; the original is dropped at the
                            // end of the arm.
                            push_way_from_coords(
                                outer_ring,
                                &mut id_alloc,
                                &mut nodes,
                                &mut ways_with_ids,
                                tags.clone(),
                                &mut min_lat,
                                &mut min_lon,
                                &mut max_lat,
                                &mut max_lon,
                            );
                        }
                    }
                }
            }

            _ => {
                // Unknown geometry type — skip.
            }
        }
    }

    let bounds = if min_lat < f64::MAX {
        Some((min_lat, min_lon, max_lat, max_lon))
    } else {
        None
    };

    Ok(OsmData::new(
        nodes,
        ways_with_ids,
        Vec::new(),
        bounds,
        poi_nodes,
        addr_nodes,
        tree_nodes,
    ))
}

// ── Overture cache ────────────────────────────────────────────────────────

/// Metadata stored beside cached Overture GeoJSON files (DOC-010: doc comment
/// now precedes the derive so rustdoc attaches it to the struct).
///
/// Carries the `overturemaps` CLI version that wrote the entry and a written-at
/// timestamp so [`overture_cache_read`] can enforce the TTL (ARC-001).
#[derive(Debug, Serialize, Deserialize)]
pub struct OvertureCacheMeta {
    /// Bounding box `[south, west, north, east]` for the cached download.
    pub bbox: [f64; 4],
    /// Overture CLI type value, such as `place`, `building`, or `segment`.
    pub cli_type: String,
    /// UTC creation timestamp (also serves as the entry's written-at time).
    pub created_at: DateTime<Utc>,
    /// GeoJSON payload size in bytes.
    pub size_bytes: u64,
    /// First non-empty line of `overturemaps --version` stdout at write time
    /// (ARC-001). Folded into the cache key so a CLI upgrade invalidates
    /// entries written under an older version. Older entries written before
    /// this field existed deserialize as an empty string.
    #[serde(default)]
    pub cli_version: String,
}

impl CacheMetaTrait for OvertureCacheMeta {
    fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
}

/// Build a [`RawCache`] rooted at `dir` for the Overture GeoJSON layout.
///
/// The Overture and Overpass caches share the same atomic write protocol and
/// orphan-skip read/list via [`RawCache`]; this helper fixes the extension
/// (`.geojson`) and metadata type ([`OvertureCacheMeta`]).
fn raw_cache(dir: &Path) -> RawCache<OvertureCacheMeta> {
    RawCache::new(dir.to_path_buf(), "geojson")
}

/// Return the Overture GeoJSON cache directory, creating it if needed.
///
/// Thin re-export of [`crate::cache::overture_cache_dir`] (DOC-014) preserved
/// for ergonomic `overture::overture_cache_dir()` call sites. The canonical
/// definition lives in [`crate::cache`]; both paths resolve to the same
/// directory and the duplication is intentional so callers that already
/// `use crate::overture` need not also import [`crate::cache`].
///
/// Priority:
/// 1. `PAR_OSM_OVERTURE_CACHE_DIR` environment variable
/// 2. `OVERTURE_CACHE_DIR` environment variable
/// 3. shared default `overture` directory under [`crate::cache::shared_cache_root`]
///
/// When using the shared default, legacy osm-to-bedrock Overture cache files are
/// migrated into the shared cache on first use. Environment overrides are never
/// migrated.
pub fn overture_cache_dir() -> PathBuf {
    crate::cache::overture_cache_dir()
}

/// Build a deterministic SHA-256 cache key from a bounding box and CLI type.
///
/// This is the legacy v1 cache key, preserved for backward compatibility with
/// external callers; it is **not** version-aware, so a CLI upgrade will reuse
/// stale entries. Internal fetch paths use [`overture_cache_key_with_version`]
/// instead (ARC-001).
///
/// Coordinates are snapped to 4 decimal places (~11 m) so small UI drags
/// reuse the same entry.
pub fn overture_cache_key(bbox: (f64, f64, f64, f64), cli_type: &str) -> String {
    let (s, w, n, e) = bbox;
    let canonical = format!("overture|{s:.4},{w:.4},{n:.4},{e:.4}|{cli_type}");
    let hash = Sha256::digest(canonical.as_bytes());
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// Build a version-aware SHA-256 cache key (ARC-001).
///
/// Like [`overture_cache_key`] but folds the `overturemaps` CLI version into
/// the canonical form so a CLI upgrade produces a different key and forces
/// a re-fetch under the new version. An empty `cli_version` is normalized
/// to `"unknown"` so probing failures still produce a stable, distinct key.
///
/// The canonical form is `overture|v2|{cli_version}|{s},{w},{n},{e}|{type}`,
/// distinct from the v1 form, so any pre-existing v1 entries simply miss
/// (re-fetch) on first read after the upgrade — accepted per ARC-001.
pub fn overture_cache_key_with_version(
    bbox: (f64, f64, f64, f64),
    cli_type: &str,
    cli_version: &str,
) -> String {
    let (s, w, n, e) = bbox;
    let version = if cli_version.is_empty() {
        "unknown"
    } else {
        cli_version
    };
    let canonical = format!("overture|v2|{version}|{s:.4},{w:.4},{n:.4},{e:.4}|{cli_type}");
    let hash = Sha256::digest(canonical.as_bytes());
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// Return cached GeoJSON for `key`, or `None` if absent, unreadable, or older
/// than `ttl` (ARC-001).
///
/// `ttl` is the maximum entry age; `None` disables TTL enforcement (read
/// whatever is on disk). When `ttl` is `Some(_)`, the paired `.meta.json`
/// is read for the `created_at` timestamp; an entry whose age exceeds `ttl`,
/// a missing/unreadable/expired meta file, or a missing geojson file all
/// yield `None` (treated as a miss → caller re-fetches).
pub fn overture_cache_read(dir: &Path, key: &str, ttl: Option<Duration>) -> Option<String> {
    let cache = raw_cache(dir);
    if let Some(ttl) = ttl {
        let meta = cache.read_meta(key);
        let created_at = match meta {
            Some(m) => m.created_at,
            None => {
                log::debug!(
                    "Overture cache miss for {key}: no readable meta (TTL cannot be enforced)"
                );
                return None;
            }
        };
        // Compare in SystemTime space so an absurdly large TTL (one that would
        // overflow chrono's internal TimeDelta) is handled correctly: a future
        // `created_at` (clock skew / tampering) is also treated as a miss.
        let created_system: SystemTime = created_at.into();
        match SystemTime::now().duration_since(created_system) {
            Ok(elapsed) if elapsed > ttl => {
                log::debug!(
                    "Overture cache miss for {key}: entry age {:.0}s exceeds TTL {:.0}s",
                    elapsed.as_secs(),
                    ttl.as_secs()
                );
                return None;
            }
            Ok(_) => {} // fresh — fall through and read the geojson
            Err(_) => {
                log::debug!("Overture cache miss for {key}: created_at is in the future");
                return None;
            }
        }
    }
    cache.read_data(key)
}

/// Atomically write `geojson` + metadata for `key` (ARC-001: stores
/// `cli_version` in the meta sidecar so TTL lookups can be paired with the
/// version that wrote the entry).
///
/// Delegates to [`RawCache::write`], which owns the QA-012 atomic protocol
/// (meta sidecar finalized first, data renamed last). The committed/visible
/// state is "both files present"; a crash before the final data rename leaves
/// meta-without-data, which [`overture_cache_read`] treats as a miss.
pub fn overture_cache_write(
    dir: &Path,
    key: &str,
    bbox: (f64, f64, f64, f64),
    cli_type: &str,
    cli_version: &str,
    geojson: &str,
) -> Result<()> {
    let (s, w, n, e) = bbox;
    let size_bytes = geojson.len() as u64;
    let meta = OvertureCacheMeta {
        bbox: [s, w, n, e],
        cli_type: cli_type.to_string(),
        created_at: Utc::now(),
        size_bytes,
        cli_version: cli_version.to_string(),
    };
    raw_cache(dir).write(key, geojson, &meta)
}

/// A single Overture cache entry returned by [`list_overture_areas`]
/// (DOC-010: doc comment now precedes the derive so rustdoc attaches it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OvertureCacheEntry {
    /// Cache key (SHA-256 hex) the entry is stored under.
    pub key: String,
    /// Bounding box `[south, west, north, east]`.
    pub bbox: [f64; 4],
    /// Overture CLI type value (e.g. `building`, `segment`).
    pub cli_type: String,
    /// UTC creation timestamp of the entry.
    pub created_at: DateTime<Utc>,
    /// GeoJSON payload size in bytes.
    pub size_bytes: u64,
    /// `overturemaps` CLI version string that wrote the entry (ARC-001).
    /// Empty for entries written before the field existed.
    #[serde(default)]
    pub cli_version: String,
}

/// List all valid Overture cache entries.
///
/// Delegates to [`RawCache::list`], which skips orphans (meta without data,
/// the QA-012 post-crash shape; or data without meta, the legacy shape).
pub fn list_overture_areas() -> Vec<OvertureCacheEntry> {
    let dir = overture_cache_dir();
    raw_cache(&dir)
        .list()
        .into_iter()
        .map(|(key, meta)| OvertureCacheEntry {
            key,
            bbox: meta.bbox,
            cli_type: meta.cli_type,
            created_at: meta.created_at,
            size_bytes: meta.size_bytes,
            cli_version: meta.cli_version,
        })
        .collect()
}

/// Clear Overture cache entries, optionally only those older than `min_age`.
///
/// Returns the number of entries deleted.
pub fn clear_overture_cache(min_age: Option<chrono::Duration>) -> Result<usize> {
    clear_overture_cache_dir(&overture_cache_dir(), min_age)
}

fn clear_overture_cache_dir(dir: &Path, min_age: Option<chrono::Duration>) -> Result<usize> {
    // Delegates age-based eviction and orphan sweep to [`RawCache::clear`].
    raw_cache(dir).clear(min_age)
}

// ── High-level fetch API ──────────────────────────────────────────────────

/// Create an empty [`OsmData`] to accumulate merged results into.
fn empty_osm_data() -> OsmData {
    OsmData::new(
        HashMap::new(),
        Vec::new(),
        Vec::new(),
        None,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
}

/// Fetch + cache + parse a single Overture theme/CLI-type pair (QA-007).
///
/// Shared by [`fetch_overture_data`] (which propagates the error) and
/// [`fetch_overture_data_best_effort`] (which logs and skips). Returns the
/// parsed [`OsmData`] for the single theme. Cache key is version-aware
/// ([`overture_cache_key_with_version`]) and reads enforce
/// [`OvertureParams::cache_ttl`] (ARC-001).
fn fetch_one_theme(
    theme: OvertureTheme,
    cli_type: &'static str,
    bbox: (f64, f64, f64, f64),
    params: &OvertureParams,
    cache_dir: &Path,
    cli_version: &str,
) -> Result<OsmData> {
    let key = overture_cache_key_with_version(bbox, cli_type, cli_version);
    let ttl = params.cache_ttl();

    let geojson = match overture_cache_read(cache_dir, &key, Some(ttl)) {
        Some(cached) => {
            log::debug!("Overture cache hit for {cli_type} (key {key})");
            cached
        }
        None => {
            log::debug!("Overture cache miss for {cli_type} — downloading");
            let fetched = fetch_geojson_for_type(cli_type, bbox, params.timeout_secs)
                .with_context(|| format!("fetching Overture data for type '{cli_type}'"))?;
            overture_cache_write(cache_dir, &key, bbox, cli_type, cli_version, &fetched)
                .with_context(|| format!("caching Overture data for type '{cli_type}'"))?;
            fetched
        }
    };

    parse_overture_geojson(&geojson, theme)
        .with_context(|| format!("parsing Overture GeoJSON for type '{cli_type}'"))
}

/// Fetch Overture Maps data for the enabled themes in `params` and normalize
/// it into a single [`OsmData`] (DOC-011: removed the duplicated summary that
/// previously appeared around the `# Errors` section).
///
/// For each CLI type belonging to each requested theme:
/// 1. Check the disk cache (version-aware key + TTL, ARC-001).
/// 2. On cache miss, invoke the `overturemaps` CLI to download GeoJSON.
/// 3. Write the result to cache with the probed CLI version.
/// 4. Parse the GeoJSON into [`OsmData`] and merge.
///
/// This function shells out to the optional `overturemaps` CLI and may perform
/// network I/O. The returned data can be merged with OSM data via
/// [`crate::sources::merge_source_data`] or fetched through the higher-level
/// [`crate::sources::fetch_map_data`] orchestrator.
///
/// # Errors
///
/// Returns an error if `params.enabled` is false, the CLI is not installed,
/// or any theme fetch or parse fails.
///
/// # Examples
///
/// ```no_run
/// use par_osm_rust::overture::{fetch_overture_data, OvertureParams, OvertureTheme};
///
/// # fn main() -> anyhow::Result<()> {
/// let bbox = (38.0, -121.0, 38.01, -120.99); // south, west, north, east
/// let params = OvertureParams {
///     enabled: true,
///     themes: vec![OvertureTheme::Place],
///     ..Default::default()
/// };
/// let mut progress = |_: f32, _: &str| {};
/// let data = fetch_overture_data(bbox, &params, &mut progress)?;
/// println!("{} ways", data.iter_ways().count());
/// # Ok(())
/// # }
/// ```
pub fn fetch_overture_data(
    bbox: (f64, f64, f64, f64),
    params: &OvertureParams,
    progress_cb: &mut dyn FnMut(f32, &str),
) -> Result<OsmData> {
    if !params.enabled {
        bail!("Overture Maps integration is not enabled");
    }
    if !is_cli_available() {
        bail!(
            "The `overturemaps` CLI is not installed.\n\
             Install it with: pip install overturemaps\n\
             Then retry."
        );
    }

    let cli_version = cached_cli_version();
    let theme_names: Vec<String> = params.themes.iter().map(|t| t.to_string()).collect();
    log::info!(
        "Starting Overture Maps fetch (bbox: {:.4},{:.4},{:.4},{:.4}, themes: {}, cli_version: {})",
        bbox.0,
        bbox.1,
        bbox.2,
        bbox.3,
        theme_names.join(", "),
        cli_version,
    );

    let cache_dir = overture_cache_dir();

    // Flatten all (theme, cli_type) pairs so we can report progress as a
    // fraction of total work.
    let pairs: Vec<(OvertureTheme, &'static str)> = params
        .themes
        .iter()
        .flat_map(|&theme| theme.cli_types().into_iter().map(move |t| (theme, t)))
        .collect();

    let total = pairs.len() as f32;
    let mut accumulated = empty_osm_data();

    for (i, (theme, cli_type)) in pairs.iter().enumerate() {
        let pct = i as f32 / total;
        progress_cb(pct, &format!("Fetching Overture {cli_type}…"));

        let data = fetch_one_theme(*theme, cli_type, bbox, params, &cache_dir, cli_version)?;
        accumulated.merge(data);
    }

    log::info!(
        "Overture Maps fetch complete ({} ways, {} POI nodes, {} address nodes)",
        accumulated.ways.len(),
        accumulated.poi_nodes.len(),
        accumulated.addr_nodes.len(),
    );
    progress_cb(1.0, "Overture data ready");
    Ok(accumulated)
}

/// Like [`fetch_overture_data`] but never fails.
///
/// - If Overture is disabled, returns empty [`OsmData`].
/// - If the CLI is unavailable, returns empty [`OsmData`] after logging a warning.
/// - If a theme fetch fails, logs a warning and skips it.
/// - If parsing a GeoJSON result fails, logs a warning and skips it.
///
/// Use this lower-level helper when callers want partial Overture data without
/// bubbling errors. Applications that need explicit fallback status should prefer
/// [`crate::sources::fetch_map_data`].
pub fn fetch_overture_data_best_effort(
    bbox: (f64, f64, f64, f64),
    params: &OvertureParams,
    progress_cb: &mut dyn FnMut(f32, &str),
) -> OsmData {
    if !params.enabled {
        return empty_osm_data();
    }
    if !is_cli_available() {
        log::warn!(
            "Overture Maps CLI not available — skipping Overture data.\n\
             Install with: pip install overturemaps"
        );
        return empty_osm_data();
    }

    let cli_version = cached_cli_version();
    let cache_dir = overture_cache_dir();

    let pairs: Vec<(OvertureTheme, &'static str)> = params
        .themes
        .iter()
        .flat_map(|&theme| theme.cli_types().into_iter().map(move |t| (theme, t)))
        .collect();

    let total = pairs.len() as f32;
    let mut accumulated = empty_osm_data();

    for (i, (theme, cli_type)) in pairs.iter().enumerate() {
        let pct = i as f32 / total;
        progress_cb(pct, &format!("Fetching Overture {cli_type}…"));

        match fetch_one_theme(*theme, cli_type, bbox, params, &cache_dir, cli_version) {
            Ok(data) => accumulated.merge(data),
            Err(e) => log::warn!("Skipping Overture type '{cli_type}': {e}"),
        }
    }

    progress_cb(1.0, "Overture data ready");
    accumulated
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Mutex;

    static PATH_LOCK: Mutex<()> = Mutex::new(());

    struct PathGuard {
        original_path: Option<OsString>,
    }

    impl Drop for PathGuard {
        fn drop(&mut self) {
            match &self.original_path {
                // SAFETY (SEC-007): env mutation became `unsafe` in Rust 1.85
                // (Edition 2024) because it is not thread-safe across the
                // whole process. This test module serializes all such
                // mutations behind `PATH_LOCK` (a single Mutex held for the
                // duration of each test that touches PATH), so no other code
                // in this crate can read or write PATH concurrently. We do
                // not pull in `temp_env` because SEC-007 forbids editing
                // Cargo.toml in this wave. The original value is restored on
                // drop so the mutation is also scoped to the test.
                Some(path) => unsafe { std::env::set_var("PATH", path) },
                None => unsafe { std::env::remove_var("PATH") },
            }
        }
    }

    fn prepend_to_path(path: &Path) -> PathGuard {
        let original_path = std::env::var_os("PATH");
        let mut paths = vec![path.to_path_buf()];
        if let Some(original) = &original_path {
            paths.extend(std::env::split_paths(original));
        }
        let joined = std::env::join_paths(paths).expect("join PATH entries");
        // SAFETY (SEC-007): see `PathGuard::drop` — caller holds `PATH_LOCK`
        // for the duration of this test, and the original value is restored
        // when the returned `PathGuard` drops.
        unsafe { std::env::set_var("PATH", joined) };

        PathGuard { original_path }
    }

    #[cfg(unix)]
    fn write_fake_overturemaps(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("overturemaps");
        std::fs::write(&path, script).expect("write fake overturemaps script");
        let mut permissions = std::fs::metadata(&path)
            .expect("fake overturemaps metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("chmod fake overturemaps script");
        path
    }

    // ── helpers ──────────────────────────────────────────────────────────

    fn point_feature(lon: f64, lat: f64, props: serde_json::Value) -> String {
        serde_json::json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": {
                    "type": "Point",
                    "coordinates": [lon, lat]
                },
                "properties": props
            }]
        })
        .to_string()
    }

    fn polygon_feature(props: serde_json::Value) -> String {
        // A simple 4-corner square polygon.
        serde_json::json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [[
                        [0.0, 0.0],
                        [0.0, 1.0],
                        [1.0, 1.0],
                        [1.0, 0.0],
                        [0.0, 0.0]
                    ]]
                },
                "properties": props
            }]
        })
        .to_string()
    }

    fn line_feature(props: serde_json::Value) -> String {
        serde_json::json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": {
                    "type": "LineString",
                    "coordinates": [
                        [0.0, 0.0],
                        [0.0, 1.0],
                        [1.0, 1.0]
                    ]
                },
                "properties": props
            }]
        })
        .to_string()
    }

    // ── Theme parsing tests ──────────────────────────────────────────────

    #[test]
    fn from_str_loose_parses_address_singular_and_plural() {
        assert_eq!(
            OvertureTheme::from_str_loose("address"),
            Some(OvertureTheme::Address)
        );
        assert_eq!(
            OvertureTheme::from_str_loose("addresses"),
            Some(OvertureTheme::Address)
        );
    }

    #[test]
    fn from_str_loose_preserves_existing_accepted_forms() {
        assert_eq!(
            OvertureTheme::from_str_loose("buildings"),
            Some(OvertureTheme::Building)
        );
        assert_eq!(
            OvertureTheme::from_str_loose("roads"),
            Some(OvertureTheme::Transportation)
        );
        assert_eq!(
            OvertureTheme::from_str_loose("landuse"),
            Some(OvertureTheme::Base)
        );
        assert_eq!(
            OvertureTheme::from_str_loose("addr"),
            Some(OvertureTheme::Address)
        );
    }

    // ── CLI tests ────────────────────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn fetch_geojson_drains_large_stderr_without_waiting_for_timeout() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        write_fake_overturemaps(
            tmp.path(),
            r#"#!/bin/sh
printf 'fake overturemaps useful error: stderr flood begins\n' >&2
i=0
while [ "$i" -lt 20000 ]; do
  printf 'stderr filler line %05d abcdefghijklmnopqrstuvwxyz\n' "$i" >&2
  i=$((i + 1))
done
printf 'fake overturemaps useful error: final diagnostic\n' >&2
exit 23
"#,
        );

        let _lock = PATH_LOCK.lock().expect("PATH lock poisoned");
        let _path_guard = prepend_to_path(tmp.path());
        let start = Instant::now();

        let err = fetch_geojson_for_type("place", (51.5, -0.13, 51.52, -0.10), 5)
            .expect_err("fake CLI should fail");

        assert!(
            start.elapsed() < Duration::from_secs(2),
            "fetch should return promptly instead of waiting for timeout; elapsed {:?}",
            start.elapsed()
        );
        let message = err.to_string();
        assert!(
            message.contains("fake overturemaps useful error"),
            "error should include useful stderr snippet, got: {message}"
        );
    }

    // ── Building tests ───────────────────────────────────────────────────

    #[test]
    fn building_with_class_height_floors() {
        let geojson = polygon_feature(serde_json::json!({
            "class": "residential",
            "height": 12.5,
            "num_floors": 4
        }));
        let data = parse_overture_geojson(&geojson, OvertureTheme::Building).unwrap();
        assert_eq!(data.ways.len(), 1);
        let tags = &data.ways[0].tags;
        assert_eq!(tags["building"], "residential");
        assert_eq!(tags["building:height"], "12.5");
        assert_eq!(tags["building:levels"], "4");
    }

    #[test]
    fn building_no_class_defaults_yes() {
        let geojson = polygon_feature(serde_json::json!({}));
        let data = parse_overture_geojson(&geojson, OvertureTheme::Building).unwrap();
        assert_eq!(data.ways.len(), 1);
        assert_eq!(data.ways[0].tags["building"], "yes");
    }

    // ── Transportation tests ─────────────────────────────────────────────

    #[test]
    fn transportation_all_fields() {
        let geojson = line_feature(serde_json::json!({
            "class": "primary",
            "names": { "primary": "Main Street" },
            "road_surface": "paved",
            "is_bridge": true,
            "is_tunnel": false
        }));
        let data = parse_overture_geojson(&geojson, OvertureTheme::Transportation).unwrap();
        assert_eq!(data.ways.len(), 1);
        let tags = &data.ways[0].tags;
        assert_eq!(tags["highway"], "primary");
        assert_eq!(tags["name"], "Main Street");
        assert_eq!(tags["surface"], "paved");
        assert_eq!(tags["bridge"], "yes");
        assert!(!tags.contains_key("tunnel"));
    }

    #[test]
    fn transportation_no_class_defaults_unclassified() {
        let geojson = line_feature(serde_json::json!({}));
        let data = parse_overture_geojson(&geojson, OvertureTheme::Transportation).unwrap();
        assert_eq!(data.ways[0].tags["highway"], "unclassified");
    }

    // ── Place tests ──────────────────────────────────────────────────────

    #[test]
    fn place_becomes_poi_node() {
        let geojson = point_feature(
            -0.1,
            51.5,
            serde_json::json!({
                "categories": { "primary": "restaurant" },
                "names": { "primary": "The Bistro" }
            }),
        );
        let data = parse_overture_geojson(&geojson, OvertureTheme::Place).unwrap();
        assert_eq!(data.poi_nodes.len(), 1);
        assert_eq!(data.poi_nodes[0].tags["amenity"], "restaurant");
        assert_eq!(data.poi_nodes[0].tags["name"], "The Bistro");
        assert_eq!(data.poi_nodes[0].source, FeatureSource::Overture);
        assert!((data.poi_nodes[0].lat - 51.5).abs() < 1e-9);
        assert!((data.poi_nodes[0].lon - -0.1).abs() < 1e-9);
    }

    // ── Base theme tests ─────────────────────────────────────────────────

    #[test]
    fn base_water_subtype_maps_to_natural_water() {
        let geojson = polygon_feature(serde_json::json!({
            "subtype": "lake",
            "class": "lake"
        }));
        let data = parse_overture_geojson(&geojson, OvertureTheme::Base).unwrap();
        assert_eq!(data.ways.len(), 1);
        assert_eq!(data.ways[0].tags["natural"], "water");
        assert_eq!(data.ways[0].tags["water"], "lake");
    }

    #[test]
    fn base_landuse_forest_subtype() {
        let geojson = polygon_feature(serde_json::json!({
            "subtype": "forest",
            "class": "forest"
        }));
        let data = parse_overture_geojson(&geojson, OvertureTheme::Base).unwrap();
        assert_eq!(data.ways.len(), 1);
        assert_eq!(data.ways[0].tags["landuse"], "forest");
    }

    // ── Address tests ────────────────────────────────────────────────────

    #[test]
    fn address_becomes_addr_node() {
        let geojson = point_feature(
            -0.2,
            51.6,
            serde_json::json!({
                "number": "42",
                "street": "Baker Street"
            }),
        );
        let data = parse_overture_geojson(&geojson, OvertureTheme::Address).unwrap();
        assert_eq!(data.addr_nodes.len(), 1);
        assert_eq!(data.addr_nodes[0].tags["addr:housenumber"], "42");
        assert_eq!(data.addr_nodes[0].tags["addr:street"], "Baker Street");
        assert_eq!(data.addr_nodes[0].source, FeatureSource::Overture);
        // Should NOT appear in poi_nodes.
        assert_eq!(data.poi_nodes.len(), 0);
    }

    // ── Edge cases ───────────────────────────────────────────────────────

    #[test]
    fn empty_feature_collection_returns_empty_osm_data() {
        let geojson = r#"{"type":"FeatureCollection","features":[]}"#;
        let data = parse_overture_geojson(geojson, OvertureTheme::Building).unwrap();
        assert!(data.nodes.is_empty());
        assert!(data.ways.is_empty());
        assert!(data.poi_nodes.is_empty());
        assert!(data.addr_nodes.is_empty());
        assert!(data.bounds.is_none());
    }

    #[test]
    fn multipolygon_produces_multiple_ways() {
        let geojson = serde_json::json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": {
                    "type": "MultiPolygon",
                    "coordinates": [
                        [[[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [0.0, 0.0]]],
                        [[[2.0, 2.0], [2.0, 3.0], [3.0, 3.0], [2.0, 2.0]]]
                    ]
                },
                "properties": { "class": "office" }
            }]
        })
        .to_string();
        let data = parse_overture_geojson(&geojson, OvertureTheme::Building).unwrap();
        assert_eq!(data.ways.len(), 2);
    }

    #[test]
    fn bounds_computed_correctly() {
        let geojson = polygon_feature(serde_json::json!({}));
        let data = parse_overture_geojson(&geojson, OvertureTheme::Building).unwrap();
        let (min_lat, min_lon, max_lat, max_lon) = data.bounds.unwrap();
        assert!((min_lat - 0.0).abs() < 1e-9);
        assert!((min_lon - 0.0).abs() < 1e-9);
        assert!((max_lat - 1.0).abs() < 1e-9);
        assert!((max_lon - 1.0).abs() < 1e-9);
    }

    // ── Determinism tests (ARC-009 / QA-010) ────────────────────────────

    #[test]
    fn parse_overture_geojson_is_deterministic_across_calls() {
        // Two parses of identical GeoJSON must produce identical synthetic
        // IDs (the per-parse allocator resets on each call). The previous
        // global AtomicI64 design made the second parse's IDs depend on the
        // first.
        let geojson = polygon_feature(serde_json::json!({}));
        let first = parse_overture_geojson(&geojson, OvertureTheme::Building).unwrap();
        let second = parse_overture_geojson(&geojson, OvertureTheme::Building).unwrap();

        let first_way_id = first.way_id_at(0).expect("first parse has a way");
        let second_way_id = second.way_id_at(0).expect("second parse has a way");
        assert_eq!(
            first_way_id, second_way_id,
            "way IDs diverged across identical parses"
        );
        assert_eq!(first.nodes.len(), second.nodes.len());
        assert_eq!(
            first
                .nodes
                .keys()
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            second
                .nodes
                .keys()
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            "node IDs diverged across identical parses"
        );
    }

    // ── Cache tests ──────────────────────────────────────────────────────

    #[test]
    fn overture_cache_key_is_deterministic() {
        let bbox = (51.5, -0.13, 51.52, -0.10);
        let k1 = overture_cache_key(bbox, "building");
        let k2 = overture_cache_key(bbox, "building");
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64, "SHA-256 hex should be 64 chars");
    }

    #[test]
    fn overture_cache_key_varies_by_theme() {
        let bbox = (51.5, -0.13, 51.52, -0.10);
        let k1 = overture_cache_key(bbox, "building");
        let k2 = overture_cache_key(bbox, "segment");
        assert_ne!(k1, k2);
    }

    #[test]
    fn overture_cache_key_with_version_is_deterministic() {
        let bbox = (51.5, -0.13, 51.52, -0.10);
        let k1 = overture_cache_key_with_version(bbox, "building", "0.4.0");
        let k2 = overture_cache_key_with_version(bbox, "building", "0.4.0");
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64, "SHA-256 hex should be 64 chars");
    }

    #[test]
    fn overture_cache_key_with_version_varies_by_cli_version() {
        // ARC-001: a CLI upgrade must invalidate the cache.
        let bbox = (51.5, -0.13, 51.52, -0.10);
        let k_old = overture_cache_key_with_version(bbox, "building", "0.4.0");
        let k_new = overture_cache_key_with_version(bbox, "building", "0.5.0");
        assert_ne!(k_old, k_new);
    }

    #[test]
    fn overture_cache_key_with_version_differs_from_legacy_key() {
        // ARC-001: the new v2 canonical form must not collide with v1.
        let bbox = (51.5, -0.13, 51.52, -0.10);
        let legacy = overture_cache_key(bbox, "building");
        let versioned = overture_cache_key_with_version(bbox, "building", "0.4.0");
        assert_ne!(legacy, versioned);
    }

    #[test]
    fn overture_cache_write_read_roundtrip() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let bbox = (51.5_f64, -0.13_f64, 51.52_f64, -0.10_f64);
        let key = overture_cache_key_with_version(bbox, "building", "test");
        let geojson = r#"{"type":"FeatureCollection","features":[]}"#;

        overture_cache_write(tmp.path(), &key, bbox, "building", "test", geojson).unwrap();
        // `None` disables TTL enforcement.
        let result = overture_cache_read(tmp.path(), &key, None);
        assert_eq!(result.as_deref(), Some(geojson));
    }

    #[test]
    fn overture_cache_read_returns_none_when_ttl_exceeded() {
        // ARC-001: an entry older than the TTL is treated as a miss.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let bbox = (51.5_f64, -0.13_f64, 51.52_f64, -0.10_f64);
        let key = overture_cache_key_with_version(bbox, "building", "test");
        let geojson = r#"{"type":"FeatureCollection","features":[]}"#;

        // Hand-write a meta file whose `created_at` is well in the past so
        // any positive TTL is exceeded.
        let meta_path = tmp.path().join(format!("{key}.meta.json"));
        let geojson_path = tmp.path().join(format!("{key}.geojson"));
        std::fs::write(&geojson_path, geojson).unwrap();
        let past = Utc::now() - chrono::Duration::days(365);
        let meta = serde_json::json!({
            "bbox": [bbox.0, bbox.1, bbox.2, bbox.3],
            "cli_type": "building",
            "created_at": past,
            "size_bytes": geojson.len() as u64,
            "cli_version": "test",
        });
        std::fs::write(&meta_path, meta.to_string()).unwrap();

        // 1-second TTL — entry is a year old, so this must miss.
        let result = overture_cache_read(tmp.path(), &key, Some(Duration::from_secs(1)));
        assert!(
            result.is_none(),
            "expired entry should be treated as a miss"
        );
    }

    #[test]
    fn overture_cache_read_returns_data_when_entry_is_fresh() {
        // ARC-001 counterpart: a freshly-written entry is a hit.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let bbox = (51.5_f64, -0.13_f64, 51.52_f64, -0.10_f64);
        let key = overture_cache_key_with_version(bbox, "building", "test");
        let geojson = r#"{"type":"FeatureCollection","features":[]}"#;

        overture_cache_write(tmp.path(), &key, bbox, "building", "test", geojson).unwrap();
        // 30-day TTL — entry is seconds old, so this must hit.
        let result = overture_cache_read(
            tmp.path(),
            &key,
            Some(Duration::from_secs(30 * 24 * 60 * 60)),
        );
        assert_eq!(result.as_deref(), Some(geojson));
    }

    #[test]
    fn overture_cache_read_returns_none_when_meta_missing_under_ttl() {
        // ARC-001: when TTL is set but the meta file is absent/unreadable,
        // we cannot enforce freshness — treat as a miss rather than serve
        // potentially stale data without a timestamp.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let bbox = (51.5_f64, -0.13_f64, 51.52_f64, -0.10_f64);
        let key = overture_cache_key_with_version(bbox, "building", "test");
        let geojson_path = tmp.path().join(format!("{key}.geojson"));
        std::fs::write(
            &geojson_path,
            r#"{"type":"FeatureCollection","features":[]}"#,
        )
        .unwrap();

        let result = overture_cache_read(tmp.path(), &key, Some(Duration::from_secs(60)));
        assert!(result.is_none(), "missing meta under TTL must miss");
    }

    // ── SEC-012 argument-injection guard ────────────────────────────────

    #[test]
    fn validate_cli_type_rejects_dash_and_whitespace() {
        // Bare theme name accepted.
        assert!(validate_cli_type("building").is_ok());
        assert!(validate_cli_type("land_use").is_ok()); // underscore, not dash

        // Empty rejected.
        assert!(validate_cli_type("").is_err());

        // Argument-injection shapes rejected (would let the value be parsed
        // as a CLI flag by overturemaps).
        assert!(validate_cli_type("--output=/etc/passwd").is_err());
        assert!(validate_cli_type("-t").is_err());
        assert!(validate_cli_type("building segment").is_err());
        assert!(validate_cli_type("building\tsegment").is_err());
        assert!(validate_cli_type("\nbuilding").is_err());
    }

    #[test]
    fn fetch_geojson_for_type_rejects_argument_injection() {
        // SEC-012: a user-controlled cli_type must not reach the CLI as a flag.
        let err = fetch_geojson_for_type("--output=/tmp/evil", (0.0, 0.0, 1.0, 1.0), 1)
            .expect_err("dashed cli_type must be rejected before spawn");
        let msg = err.to_string();
        assert!(
            msg.contains("SEC-012") || msg.contains("argument-injection"),
            "error should mention the SEC-012 guard, got: {msg}"
        );
    }
}
