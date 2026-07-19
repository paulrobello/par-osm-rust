//! Disk cache for raw Overpass XML responses.
//!
//! Layout: shared Overpass cache directory with `{sha256}.xml` + `{sha256}.meta.json`.
//! Legacy key: SHA-256 of `"v2|{s:.4},{w:.4},{n:.4},{e:.4}|roads={},buildings={},water={},landuse={},railways={}"`.
//! URL-aware key: SHA-256 of `"overpass-url-v3|{canonical_url}|{s:.4},{w:.4},{n:.4},{e:.4}|roads={},buildings={},water={},landuse={},railways={}"`.
//!
//! Reads, writes, listing, and clearing delegate to the generic
//! [`crate::cache_store::RawCache`] helper. That helper owns the QA-012
//! atomic write protocol (meta-first, data-last) so the OSM and Overture
//! caches share one correct implementation instead of two near-duplicates.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use url::Url;

use crate::cache_store::{CacheMeta as CacheMetaTrait, RawCache, to_hex};
use crate::filter::FeatureFilter;

// ── Schema versions ────────────────────────────────────────────────────────
//
// Two versioning schemes coexist for backward compatibility — do not conflate
// them (QA-019). Each constant is bumped independently, only when that
// family's key canonicalization or [`CacheMeta`] layout changes in a
// backward-incompatible way:
//
// * `CACHE_SCHEMA_VERSION` (= 2) seeds the legacy bbox+filter cache key
//   produced by [`cache_key`]. The key prefix is `v2|`. Existing legacy
//   entries with a different prefix simply miss and re-write.
// * `URL_AWARE_CACHE_SCHEMA_VERSION` (= 3) seeds the URL-aware cache key
//   produced by [`cache_key_for_url`]. The key prefix is `overpass-url-v3|`.
//   It started at 3 — deliberately ahead of the legacy family — so URL-aware
//   keys can never collide with legacy keys (different prefix ⇒ different
//   SHA-256 even for identical bbox/filter/URL inputs).
//
// The values are intentionally independent; do not "unify" them into a single
// constant. Existing caches with stale versions always miss and re-write
// rather than silently serving stale data.
const CACHE_SCHEMA_VERSION: u8 = 2;
const URL_AWARE_CACHE_SCHEMA_VERSION: u8 = 3;
const URL_AWARE_CACHE_PREFIX: &str = "overpass-url";

// ── Types ──────────────────────────────────────────────────────────────────

/// A single entry returned by [`list_areas`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// SHA-256 hex cache key the entry is stored under.
    pub key: String,
    /// Bounding box `[south, west, north, east]` covered by the cached XML.
    pub bbox: [f64; 4],
    /// Feature filter in effect when the entry was written.
    pub filter: FeatureFilter,
    /// UTC timestamp at which the entry was written.
    pub created_at: DateTime<Utc>,
    /// Size of the cached XML payload in bytes.
    pub size_bytes: u64,
}

/// Serialised metadata stored alongside the `.xml` file.
#[derive(Debug, Serialize, Deserialize)]
struct CacheMeta {
    /// Bounding box `[south, west, north, east]` covered by the cached XML.
    bbox: [f64; 4],
    /// Feature filter in effect when the entry was written.
    filter: FeatureFilter,
    /// UTC timestamp at which the entry was written.
    created_at: DateTime<Utc>,
    /// Size of the cached XML payload in bytes.
    size_bytes: u64,
    /// Canonical Overpass endpoint URL, present on URL-aware entries and
    /// absent on legacy entries. Used by [`read_for_url`] /
    /// [`find_containing_for_url`] to isolate entries written for one mirror
    /// from another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    overpass_url: Option<String>,
}

impl CacheMetaTrait for CacheMeta {
    fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
}

/// Build a [`RawCache`] rooted at `dir` for the Overpass XML layout.
fn raw_cache(dir: &std::path::Path) -> RawCache<CacheMeta> {
    RawCache::new(dir.to_path_buf(), "xml")
}

// ── Cache directory ────────────────────────────────────────────────────────

/// Return the persistent Overpass XML cache directory, creating it if needed.
pub fn cache_dir() -> PathBuf {
    crate::cache::overpass_cache_dir()
}

// ── Cache key ──────────────────────────────────────────────────────────────

/// Build a deterministic SHA-256 cache key from a bounding box and feature filter.
///
/// Coords are snapped to 4 decimal places (~11 m) so small UI drags reuse the
/// same entry.
///
/// **Deprecated** (ARC-011 / QA-011): this is the legacy bbox+filter key and
/// does not distinguish between Overpass endpoints, so an entry written for
/// one mirror is served for another. Prefer the URL-aware
/// [`cache_key_for_url`], which folds the (canonicalized) Overpass endpoint
/// into the hash. The legacy family is retained for downstream compatibility
/// with `osm-to-bedrock` / `osm-world` and will be removed in a future major
/// release.
#[deprecated(
    since = "0.2.0",
    note = "use the URL-aware `cache_key_for_url` instead; legacy keys do not isolate by Overpass endpoint"
)]
pub fn cache_key(bbox: (f64, f64, f64, f64), filter: &FeatureFilter) -> String {
    let (s, w, n, e) = bbox;
    let canonical = format!(
        "v{}|{:.4},{:.4},{:.4},{:.4}|roads={},buildings={},water={},landuse={},railways={}",
        CACHE_SCHEMA_VERSION,
        s,
        w,
        n,
        e,
        u8::from(filter.roads),
        u8::from(filter.buildings),
        u8::from(filter.water),
        u8::from(filter.landuse),
        u8::from(filter.railways),
    );
    let hash = Sha256::digest(canonical.as_bytes());
    to_hex(&hash)
}

/// Build a deterministic URL-aware SHA-256 cache key from bounding box,
/// feature filter, and Overpass endpoint.
///
/// The URL is canonicalized before hashing so insignificant differences such
/// as surrounding whitespace, URL fragments, and explicit default ports do not
/// create duplicate entries. This intentionally uses a distinct schema prefix
/// from [`cache_key`] so endpoint-specific entries cannot collide with legacy
/// bbox/filter-only entries.
pub fn cache_key_for_url(
    bbox: (f64, f64, f64, f64),
    filter: &FeatureFilter,
    overpass_url: &str,
) -> String {
    let (s, w, n, e) = bbox;
    let source = canonical_overpass_url(overpass_url);
    let canonical = format!(
        "{URL_AWARE_CACHE_PREFIX}-v{URL_AWARE_CACHE_SCHEMA_VERSION}|{source}|{:.4},{:.4},{:.4},{:.4}|roads={},buildings={},water={},landuse={},railways={}",
        s,
        w,
        n,
        e,
        u8::from(filter.roads),
        u8::from(filter.buildings),
        u8::from(filter.water),
        u8::from(filter.landuse),
        u8::from(filter.railways),
    );
    let hash = Sha256::digest(canonical.as_bytes());
    to_hex(&hash)
}

fn canonical_overpass_url(overpass_url: &str) -> String {
    let trimmed = overpass_url.trim();
    let Ok(mut parsed) = Url::parse(trimmed) else {
        return trimmed.to_string();
    };

    parsed.set_fragment(None);
    let is_default_port = matches!(
        (parsed.scheme(), parsed.port()),
        ("https", Some(443)) | ("http", Some(80))
    );
    if is_default_port {
        let _ = parsed.set_port(None);
    }

    parsed.to_string()
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Return cached XML for `key`, or `None` if not present or unreadable.
///
/// `key` must match the cache's `[0-9a-zA-Z_-]` alphabet (SEC-105) — the
/// crate's [`cache_key`] / [`cache_key_for_url`] produce SHA-256 hex digests
/// that satisfy this; a downstream app forwarding an untrusted string gets a
/// `None` (treated as a miss) for keys outside the alphabet.
///
/// **Deprecated** (ARC-011 / QA-011): this is the legacy bbox+filter read and
/// does not check the entry's Overpass endpoint. Prefer [`read_for_url`],
/// which gates the read on the entry's stored `overpass_url` so an entry
/// written for one mirror is never served for another.
#[deprecated(
    since = "0.2.0",
    note = "use the URL-aware `read_for_url` instead; legacy reads do not isolate by Overpass endpoint"
)]
pub fn read(key: &str) -> Option<String> {
    read_from(&cache_dir(), key)
}

/// Return cached XML for `key` only when its metadata matches `overpass_url`.
///
/// `key` must match the cache's `[0-9a-zA-Z_-]` alphabet (SEC-105); an
/// invalid key yields `None`.
pub fn read_for_url(key: &str, overpass_url: &str) -> Option<String> {
    read_from_for_url(&cache_dir(), key, overpass_url)
}

/// Atomically write `xml` + metadata for `key`.
///
/// `key` must match the cache's `[0-9a-zA-Z_-]` alphabet (SEC-105) — the
/// crate's [`cache_key`] / [`cache_key_for_url`] produce SHA-256 hex digests
/// that satisfy this; an out-of-alphabet key returns `Err` rather than
/// writing outside the cache directory.
///
/// **Deprecated** (ARC-011 / QA-011): this is the legacy bbox+filter write
/// and records no Overpass endpoint in the metadata. Prefer [`write_for_url`],
/// which stores the canonical `overpass_url` in the sidecar so subsequent
/// reads via [`read_for_url`] can isolate entries per mirror.
///
/// # Errors
///
/// Returns `Err` if `key` fails the alphabet check, if metadata serialization
/// fails, or if any I/O step in the atomic write-then-rename protocol fails.
#[deprecated(
    since = "0.2.0",
    note = "use the URL-aware `write_for_url` instead; legacy writes do not record the Overpass endpoint"
)]
pub fn write(
    key: &str,
    bbox: (f64, f64, f64, f64),
    filter: &FeatureFilter,
    xml: &str,
) -> Result<()> {
    write_to(&cache_dir(), key, bbox, filter, xml)
}

/// Atomically write `xml` + URL-aware metadata for `key`.
///
/// `key` must match the cache's `[0-9a-zA-Z_-]` alphabet (SEC-105); an
/// out-of-alphabet key returns `Err`.
///
/// # Errors
///
/// Returns `Err` if `key` fails the alphabet check, if metadata serialization
/// fails, or if any I/O step in the atomic write-then-rename protocol fails.
pub fn write_for_url(
    key: &str,
    bbox: (f64, f64, f64, f64),
    filter: &FeatureFilter,
    xml: &str,
    overpass_url: &str,
) -> Result<()> {
    write_to_for_url(&cache_dir(), key, bbox, filter, xml, overpass_url)
}

/// List all valid (paired) cache entries in the default cache directory.
pub fn list_areas() -> Vec<CacheEntry> {
    list_areas_in(&cache_dir())
}

/// Delete cache entries older than `min_age` (or all if `None`).
/// Returns the number of entries deleted.
///
/// # Errors
///
/// Returns `Err` if reading or deleting an entry's files fails. A missing
/// cache directory is not an error and returns `Ok(0)`.
pub fn clear(min_age: Option<chrono::Duration>) -> Result<usize> {
    clear_dir(&cache_dir(), min_age)
}

// ── Internal helpers (used directly by tests via explicit path arg) ────────
//
// Each helper constructs a [`RawCache`] rooted at `dir` and delegates the
// protocol (atomic write, orphan-skip read/list, age-based clear) to it.
// Keeping the `&Path`-based private signatures preserves the public API and
// the test helpers while collapsing the duplicated logic into one place.

fn read_from(dir: &std::path::Path, key: &str) -> Option<String> {
    raw_cache(dir).read_data(key)
}

fn read_from_for_url(dir: &std::path::Path, key: &str, overpass_url: &str) -> Option<String> {
    let source = canonical_overpass_url(overpass_url);
    if meta_matches_overpass_url(dir, key, &source) {
        read_from(dir, key)
    } else {
        None
    }
}

fn write_to(
    dir: &std::path::Path,
    key: &str,
    bbox: (f64, f64, f64, f64),
    filter: &FeatureFilter,
    xml: &str,
) -> Result<()> {
    write_to_with_overpass_url(dir, key, bbox, filter, xml, None)
}

fn write_to_for_url(
    dir: &std::path::Path,
    key: &str,
    bbox: (f64, f64, f64, f64),
    filter: &FeatureFilter,
    xml: &str,
    overpass_url: &str,
) -> Result<()> {
    let source = canonical_overpass_url(overpass_url);
    write_to_with_overpass_url(dir, key, bbox, filter, xml, Some(source))
}

fn write_to_with_overpass_url(
    dir: &std::path::Path,
    key: &str,
    bbox: (f64, f64, f64, f64),
    filter: &FeatureFilter,
    xml: &str,
    overpass_url: Option<String>,
) -> Result<()> {
    let (s, w, n, e) = bbox;
    let size_bytes = xml.len() as u64;
    let meta = CacheMeta {
        bbox: [s, w, n, e],
        filter: filter.clone(),
        created_at: Utc::now(),
        size_bytes,
        overpass_url,
    };
    // Delegates to the QA-012 atomic protocol (meta first, data last) owned
    // by [`RawCache::write`].
    raw_cache(dir).write(key, xml, &meta)
}

fn read_meta_from(dir: &std::path::Path, key: &str) -> Option<CacheMeta> {
    raw_cache(dir).read_meta(key)
}

fn meta_matches_overpass_url(dir: &std::path::Path, key: &str, canonical_url: &str) -> bool {
    matches!(
        read_meta_from(dir, key).and_then(|meta| meta.overpass_url),
        Some(source) if source == canonical_url
    )
}

fn list_areas_in(dir: &std::path::Path) -> Vec<CacheEntry> {
    list_metas_in(dir)
        .into_iter()
        .map(|(key, meta)| CacheEntry {
            key,
            bbox: meta.bbox,
            filter: meta.filter,
            created_at: meta.created_at,
            size_bytes: meta.size_bytes,
        })
        .collect()
}

/// Crate-internal listing that returns the full deserialized [`CacheMeta`]
/// for each paired cache entry (ARC-112 ≡ QA-111).
///
/// The public [`list_areas_in`] projects each meta down to the public
/// [`CacheEntry`] shape (which omits `overpass_url`); the URL-aware
/// containment lookup [`find_containing_in_for_url`] consumes the full meta
/// directly so it does not need to re-read the meta file per candidate.
/// (`meta_matches_overpass_url` remains for the single-key URL-aware read
/// path; this listing is the listing-path dedup.)
fn list_metas_in(dir: &std::path::Path) -> Vec<(String, CacheMeta)> {
    raw_cache(dir).list()
}

// ── Containment lookup ─────────────────────────────────────────────────────

/// Return cached XML for the first entry whose bbox fully contains `bbox`
/// and whose filter exactly matches `filter`.
///
/// Containment: cached_s ≤ req_s && cached_w ≤ req_w && cached_n ≥ req_n && cached_e ≥ req_e
///
/// **Deprecated** (ARC-011 / QA-011): this is the legacy bbox+filter
/// containment lookup and does not check the entry's Overpass endpoint.
/// Prefer [`find_containing_for_url`], which additionally requires the
/// entry's stored `overpass_url` to match so an entry written for one mirror
/// is never served for another. The crate's own fetch path
/// ([`crate::overpass`]) already uses the URL-aware family.
#[deprecated(
    since = "0.2.0",
    note = "use the URL-aware `find_containing_for_url` instead; legacy lookups do not isolate by Overpass endpoint"
)]
pub fn find_containing(bbox: (f64, f64, f64, f64), filter: &FeatureFilter) -> Option<String> {
    find_containing_in(&cache_dir(), bbox, filter)
}

/// Return cached XML for a containing entry only when its metadata matches
/// `overpass_url`.
pub fn find_containing_for_url(
    bbox: (f64, f64, f64, f64),
    filter: &FeatureFilter,
    overpass_url: &str,
) -> Option<String> {
    find_containing_in_for_url(&cache_dir(), bbox, filter, overpass_url)
}

fn find_containing_in(
    dir: &std::path::Path,
    bbox: (f64, f64, f64, f64),
    filter: &FeatureFilter,
) -> Option<String> {
    let (req_s, req_w, req_n, req_e) = bbox;
    for entry in list_areas_in(dir) {
        let [cs, cw, cn, ce] = entry.bbox;
        let contained = cs <= req_s && cw <= req_w && cn >= req_n && ce >= req_e;
        let filter_matches = entry.filter == *filter;
        if contained && filter_matches {
            return read_from(dir, &entry.key);
        }
    }
    None
}

fn find_containing_in_for_url(
    dir: &std::path::Path,
    bbox: (f64, f64, f64, f64),
    filter: &FeatureFilter,
    overpass_url: &str,
) -> Option<String> {
    // ARC-112 (≡ QA-111): the metas are already in memory from
    // `list_metas_in`, so filter them directly on containment + URL match.
    // The previous implementation called `list_areas_in` (which dropped the
    // `overpass_url` field) and then re-read + re-parsed each candidate's
    // meta file via `meta_matches_overpass_url` — doubling the metadata I/O
    // per lookup. The data file is still read exactly once for the match.
    let source = canonical_overpass_url(overpass_url);
    let (req_s, req_w, req_n, req_e) = bbox;
    for (key, meta) in list_metas_in(dir) {
        let [cs, cw, cn, ce] = meta.bbox;
        let contained = cs <= req_s && cw <= req_w && cn >= req_n && ce >= req_e;
        let filter_matches = meta.filter == *filter;
        let source_matches = meta.overpass_url.as_deref() == Some(source.as_str());
        if contained && filter_matches && source_matches {
            return read_from(dir, &key);
        }
    }
    None
}

fn clear_dir(dir: &std::path::Path, min_age: Option<chrono::Duration>) -> Result<usize> {
    // Delegates the age-based eviction and orphan sweep to [`RawCache::clear`].
    raw_cache(dir).clear(min_age)
}

#[cfg(test)]
#[allow(deprecated)] // tests intentionally exercise the legacy cache_key family for coverage
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn with_cache_dir() -> TempDir {
        tempfile::tempdir().expect("tmpdir")
    }

    fn all_on() -> FeatureFilter {
        FeatureFilter::default()
    }

    fn roads_only() -> FeatureFilter {
        FeatureFilter {
            roads: true,
            buildings: false,
            water: false,
            landuse: false,
            railways: false,
        }
    }

    /// Helper: write a meta file with a synthetic created_at timestamp.
    fn write_meta_at(dir: &std::path::Path, key: &str, created_at: DateTime<Utc>) {
        let meta = CacheMeta {
            bbox: [51.5, -0.13, 51.52, -0.10],
            filter: FeatureFilter::default(),
            created_at,
            size_bytes: 100,
            overpass_url: None,
        };
        // Also write a dummy .xml so clear() has both files
        std::fs::write(dir.join(format!("{key}.xml")), b"<osm/>").unwrap();
        let meta_path = dir.join(format!("{key}.meta.json"));
        std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();
    }

    #[test]
    fn cache_key_is_deterministic() {
        let bbox = (51.5, -0.13, 51.52, -0.10);
        let k1 = cache_key(bbox, &all_on());
        let k2 = cache_key(bbox, &all_on());
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64, "SHA-256 hex should be 64 chars");
    }

    #[test]
    fn cache_key_snaps_coordinates() {
        // Differ by < 0.00005° (half of 0.0001°) → same key
        let bbox1 = (51.50001, -0.13000, 51.52001, -0.10000);
        let bbox2 = (51.50002, -0.13002, 51.52002, -0.10001);
        assert_eq!(cache_key(bbox1, &all_on()), cache_key(bbox2, &all_on()));
    }

    #[test]
    fn cache_key_varies_by_filter() {
        let bbox = (51.5, -0.13, 51.52, -0.10);
        let all_key = cache_key(bbox, &all_on());
        let roads_key = cache_key(bbox, &roads_only());
        assert_ne!(all_key, roads_key);
    }

    #[test]
    fn cache_key_for_url_varies_by_overpass_url() {
        let bbox = (51.5, -0.13, 51.52, -0.10);
        let default_key =
            cache_key_for_url(bbox, &all_on(), "https://overpass-api.de/api/interpreter");
        let mirror_key = cache_key_for_url(
            bbox,
            &all_on(),
            "https://overpass.kumi.systems/api/interpreter",
        );
        assert_ne!(default_key, mirror_key);
    }

    #[test]
    fn cache_key_for_url_is_deterministic_for_default_url() {
        let bbox = (51.5, -0.13, 51.52, -0.10);
        let k1 = cache_key_for_url(bbox, &all_on(), "https://overpass-api.de/api/interpreter");
        let k2 = cache_key_for_url(
            bbox,
            &all_on(),
            " https://overpass-api.de:443/api/interpreter#ignored ",
        );
        assert_eq!(k1, k2);
        assert_ne!(k1, cache_key(bbox, &all_on()));
    }

    #[test]
    fn write_for_url_then_read_for_other_url_returns_none() {
        let tmp = with_cache_dir();
        let bbox = (51.5_f64, -0.13_f64, 51.52_f64, -0.10_f64);
        let default_url = "https://overpass-api.de/api/interpreter";
        let mirror_url = "https://overpass.kumi.systems/api/interpreter";
        let key = cache_key_for_url(bbox, &all_on(), default_url);
        let xml = "<osm><node id='1'/></osm>";

        write_to_for_url(tmp.path(), &key, bbox, &all_on(), xml, default_url).unwrap();

        assert_eq!(
            read_from_for_url(tmp.path(), &key, default_url).as_deref(),
            Some(xml)
        );
        assert!(read_from_for_url(tmp.path(), &key, mirror_url).is_none());
    }

    #[test]
    fn find_containing_for_url_ignores_other_overpass_url() {
        let tmp = with_cache_dir();
        let large_bbox = (51.5_f64, -0.13_f64, 51.52_f64, -0.10_f64);
        let small_bbox = (51.505, -0.125, 51.515, -0.105);
        let default_url = "https://overpass-api.de/api/interpreter";
        let mirror_url = "https://overpass.kumi.systems/api/interpreter";
        let key = cache_key_for_url(large_bbox, &all_on(), default_url);
        let xml = "<osm><node id='1'/></osm>";

        write_to_for_url(tmp.path(), &key, large_bbox, &all_on(), xml, default_url).unwrap();

        assert_eq!(
            find_containing_in_for_url(tmp.path(), small_bbox, &all_on(), default_url).as_deref(),
            Some(xml)
        );
        assert!(
            find_containing_in_for_url(tmp.path(), small_bbox, &all_on(), mirror_url).is_none()
        );
    }

    #[test]
    fn write_then_read_roundtrip() {
        let tmp = with_cache_dir();
        let key = "testkey123";
        let xml = "<osm><node id='1'/></osm>";
        let bbox = (51.5_f64, -0.13_f64, 51.52_f64, -0.10_f64);

        write_to(tmp.path(), key, bbox, &FeatureFilter::default(), xml).unwrap();
        let got = read_from(tmp.path(), key);
        assert_eq!(got.as_deref(), Some(xml));
    }

    #[test]
    fn clear_all_removes_both_files() {
        let tmp = with_cache_dir();
        let key = "aabbcc";
        write_to(
            tmp.path(),
            key,
            (51.5, -0.13, 51.52, -0.10),
            &FeatureFilter::default(),
            "<osm/>",
        )
        .unwrap();

        let deleted = clear_dir(tmp.path(), None).unwrap();
        assert_eq!(deleted, 1);
        assert!(!tmp.path().join(format!("{key}.xml")).exists());
        assert!(!tmp.path().join(format!("{key}.meta.json")).exists());
    }

    #[test]
    fn clear_by_age_keeps_fresh_entries() {
        let tmp = with_cache_dir();
        let now = Utc::now();
        let old_key = "oldentry0000000000000000000000000000000000000000000000000000000a";
        let fresh_key = "freshentry000000000000000000000000000000000000000000000000000b";

        write_meta_at(tmp.path(), old_key, now - chrono::Duration::hours(2));
        write_meta_at(tmp.path(), fresh_key, now - chrono::Duration::minutes(30));

        let deleted = clear_dir(tmp.path(), Some(chrono::Duration::hours(1))).unwrap();
        assert_eq!(deleted, 1, "only the 2h-old entry should be deleted");
        assert!(!tmp.path().join(format!("{old_key}.xml")).exists());
        assert!(tmp.path().join(format!("{fresh_key}.xml")).exists());
    }

    #[test]
    fn find_containing_returns_none_when_empty() {
        let tmp = with_cache_dir();
        let result = find_containing_in(tmp.path(), (51.51, -0.12, 51.515, -0.11), &all_on());
        assert!(result.is_none());
    }

    #[test]
    fn find_containing_returns_xml_when_bbox_contained() {
        let tmp = with_cache_dir();
        let large_bbox = (51.5_f64, -0.13_f64, 51.52_f64, -0.10_f64);
        let key = cache_key(large_bbox, &all_on());
        let xml = "<osm><node id='1'/></osm>";
        write_to(tmp.path(), &key, large_bbox, &all_on(), xml).unwrap();

        // Sub-area fully inside the large bbox
        let small_bbox = (51.505, -0.125, 51.515, -0.105);
        let result = find_containing_in(tmp.path(), small_bbox, &all_on());
        assert_eq!(result.as_deref(), Some(xml));
    }

    #[test]
    fn find_containing_returns_none_when_not_contained() {
        let tmp = with_cache_dir();
        let cached_bbox = (51.5_f64, -0.13_f64, 51.52_f64, -0.10_f64);
        let key = cache_key(cached_bbox, &all_on());
        write_to(tmp.path(), &key, cached_bbox, &all_on(), "<osm/>").unwrap();

        // Requested bbox extends outside the cached one
        let outside_bbox = (51.49, -0.13, 51.52, -0.10);
        let result = find_containing_in(tmp.path(), outside_bbox, &all_on());
        assert!(result.is_none());
    }

    #[test]
    fn find_containing_returns_none_on_filter_mismatch() {
        let tmp = with_cache_dir();
        let bbox = (51.5_f64, -0.13_f64, 51.52_f64, -0.10_f64);
        let key = cache_key(bbox, &all_on());
        write_to(tmp.path(), &key, bbox, &all_on(), "<osm/>").unwrap();

        let small_bbox = (51.505, -0.125, 51.515, -0.105);
        let result = find_containing_in(tmp.path(), small_bbox, &roads_only());
        assert!(result.is_none()); // filter mismatch → None
    }

    #[test]
    fn read_returns_none_for_orphan_meta_without_data() {
        // QA-012: under the meta-first atomic protocol, a crash before the
        // final data rename leaves meta-without-data. `read_from` must treat
        // that as a miss (the data file is absent), and `list_areas_in` must
        // skip it.
        let tmp = with_cache_dir();
        let key = "orphankey0000000000000000000000000000000000000000000000000000ff";

        // Hand-write only the meta sidecar — no .xml data file.
        let meta = CacheMeta {
            bbox: [51.5, -0.13, 51.52, -0.10],
            filter: FeatureFilter::default(),
            created_at: Utc::now(),
            size_bytes: 42,
            overpass_url: None,
        };
        let meta_path = tmp.path().join(format!("{key}.meta.json"));
        std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();

        assert!(
            read_from(tmp.path(), key).is_none(),
            "meta-without-data orphan must miss on read"
        );
        let listed = list_areas_in(tmp.path());
        assert!(
            listed.iter().all(|e| e.key != key),
            "meta-without-data orphan must not appear in list_areas_in"
        );
    }
}
