//! Overture GeoJSON on-disk cache: types, key derivation, R/W, listing, clear.
//!
//! Pure (no network). Owned by [`super`] and re-exported at
//! `crate::overture::*` (ARC-007 / QA-009). [`OvertureParams`] lives here
//! because its `cache_ttl_secs` field is paired with
//! [`OVERTURE_CACHE_DEFAULT_TTL_SECS`] and the [`OvertureParams::cache_ttl`]
//! resolution.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::cache_store::{CacheMeta as CacheMetaTrait, RawCache};

use super::theme::{OvertureTheme, ThemePriority};

/// Default cache entry TTL when [`OvertureParams::cache_ttl_secs`] is `None`:
/// ~30 days. Entries older than this are treated as misses and re-fetched
/// (ARC-001).
const OVERTURE_CACHE_DEFAULT_TTL_SECS: u64 = 30 * 24 * 60 * 60;

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
    pub priority: std::collections::HashMap<OvertureTheme, ThemePriority>,
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
            priority: std::collections::HashMap::new(),
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
///
/// `key` must match the cache's `[0-9a-zA-Z_-]` alphabet (SEC-105); the
/// crate's [`overture_cache_key`] / [`overture_cache_key_with_version`]
/// produce SHA-256 hex digests that satisfy this. An out-of-alphabet key
/// yields `None` (no path is built, no file touched).
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
/// `key` must match the cache's `[0-9a-zA-Z_-]` alphabet (SEC-105); the
/// crate's [`overture_cache_key`] / [`overture_cache_key_with_version`]
/// produce SHA-256 hex digests that satisfy this. An out-of-alphabet key
/// returns `Err` without touching the filesystem.
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
