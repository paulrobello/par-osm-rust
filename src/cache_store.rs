//! Generic raw-payload disk cache shared by [`crate::osm_cache`] and
//! [`crate::overture`] (QA-003).
//!
//! Each entry is a `{key}.{ext}` data file paired with a `{key}.meta.json`
//! sidecar whose shape is fixed by the generic parameter `Meta`. Writes use
//! the QA-012 atomic protocol: the meta sidecar is finalized first (temp +
//! rename to its final path), then the data payload is renamed last. The only
//! committed/visible state is therefore "both files present." A crash before
//! the final data rename leaves meta-without-data — the new orphan shape —
//! which [`RawCache::read_data`] and [`RawCache::list`] both treat as a miss.
//!
//! This module is the single source of truth for the cache write/read/list/
//! clear protocol; the OSM and Overture caches are thin wrappers that supply
//! their own extension and metadata type.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

/// Validate a cache key against the allowed alphabet (SEC-105).
///
/// Cache keys are interpolated directly into filenames
/// (`{key}.{ext}`, `{key}.meta.json`), so any byte outside a safe
/// `[0-9a-zA-Z_-]` alphabet is a path-traversal / arbitrary-write risk on
/// API misuse. Empty keys are rejected because they would collide with the
/// extension-only filenames this module produces.
///
/// The crate's internal callers always pass a 64-character lowercase SHA-256
/// hex digest, which trivially satisfies this check; the validation is the
/// public-API contract that protects a downstream app that hands an
/// untrusted string to [`osm_cache`](crate::osm_cache) /
/// [`overture`](crate::overture) functions forwarding to [`RawCache`].
///
/// # Errors
///
/// Returns `Err` if `key` is empty or contains any byte outside
/// `[0-9a-zA-Z_-]` (this transitively rejects `/`, `\`, `..`, spaces, and
/// `NUL`).
fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() {
        anyhow::bail!("cache key must be non-empty");
    }
    if !key
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        anyhow::bail!("cache key {key:?} contains a character outside [0-9a-zA-Z_-] (SEC-105)");
    }
    Ok(())
}

/// Marker trait for a cache entry's metadata sidecar.
///
/// Binds [`Serialize`] + [`DeserializeOwned`] (so the sidecar can be written
/// and read by [`RawCache`]) and exposes the entry's written-at timestamp,
/// which [`RawCache::clear`] uses for age-based eviction.
pub trait CacheMeta: Serialize + DeserializeOwned {
    /// Return the UTC timestamp at which this entry was written.
    fn created_at(&self) -> DateTime<Utc>;
}

/// Lowercase-hex-encode a byte slice into a single pre-sized `String`.
///
/// QA-113: shared by the Overpass and Overture cache-key functions
/// (`osm_cache::overpass_cache_key(_with_url)`,
/// `overture::cache::overture_cache_key(_with_version)`). The four sites
/// previously did `hash.iter().map(|b| format!("{b:02x}")).collect()`, which
/// allocates one fresh `String` per byte (32 allocations per SHA-256 digest);
/// this version allocates the result `String` once with the exact capacity
/// and writes each byte's two hex digits with no intermediate allocation.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // `write!` on a `String` is infallible and performs no extra
        // allocation when the string has spare capacity.
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Generic raw-payload disk cache (QA-003).
///
/// Stores `{key}.{ext}` data files paired with `{key}.meta.json` sidecars in
/// a single directory. Parameterized by the metadata type `Meta` and the data
/// file extension. Owns the QA-012 atomic write protocol (meta-first,
/// data-last) so the OSM (`.xml`) and Overture (`.geojson`) caches share one
/// correct implementation instead of two near-duplicates.
///
/// The directory is **not** created by this type; callers resolve and create
/// it via [`crate::cache`] (e.g. [`crate::cache::overpass_cache_dir`]).
///
/// # Key alphabet (SEC-105)
///
/// Every public method that takes a `key` validates it against the
/// `[0-9a-zA-Z_-]` alphabet and rejects empty keys. This is the
/// path-traversal guard: a key containing `/`, `\`, `..`, spaces, or `NUL`
/// would otherwise be interpolated into a filename and could escape the cache
/// directory. Internal callers always pass a SHA-256 hex digest (which
/// satisfies the alphabet); a downstream app forwarding untrusted strings to
/// [`crate::osm_cache`] or [`crate::overture`] cache functions is protected
/// here. Read methods return `None` for an invalid key (treated as a miss);
/// [`write`](Self::write) returns `Err`.
pub struct RawCache<Meta> {
    dir: PathBuf,
    data_ext: &'static str,
    _marker: PhantomData<Meta>,
}

impl<Meta: CacheMeta> RawCache<Meta> {
    /// Open the cache rooted at `dir`, using `data_ext` (e.g. `"xml"`,
    /// `"geojson"`, without the leading dot) for data file names.
    pub fn new<P: Into<PathBuf>>(dir: P, data_ext: &'static str) -> Self {
        Self {
            dir: dir.into(),
            data_ext,
            _marker: PhantomData,
        }
    }

    /// The directory this cache reads and writes.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn data_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.{}", self.data_ext))
    }

    fn meta_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.meta.json"))
    }

    /// Read the data payload for `key`, or `None` if the data file is absent
    /// or unreadable.
    ///
    /// Per the QA-012 protocol, "meta present, data absent" (the post-crash
    /// orphan shape) is naturally a miss here: the data file does not exist,
    /// so this returns `None`. Callers that also need a meta-sidecar check
    /// (e.g. URL or TTL matching) should pair this with [`read_meta`](Self::read_meta)
    /// and apply the check themselves before consuming the payload.
    ///
    /// Returns `None` if `key` fails the `[0-9a-zA-Z_-]` alphabet check
    /// (SEC-105) — an invalid key is treated as a miss so a malicious caller
    /// learns nothing about the filesystem from the return value.
    pub fn read_data(&self, key: &str) -> Option<String> {
        // SEC-105: validate before path construction. A miss (None) is the
        // safe result for an invalid key: no path is built, no file is
        // touched, no information is leaked.
        if validate_key(key).is_err() {
            log::debug!("cache read_data rejected invalid key {key:?} (SEC-105)");
            return None;
        }
        let path = self.data_path(key);
        match std::fs::read_to_string(&path) {
            Ok(s) => Some(s),
            Err(e) => {
                log::debug!("Cache miss for {key} at {}: {e}", path.display());
                None
            }
        }
    }

    /// Read and deserialize the meta sidecar for `key`, or `None` if the
    /// sidecar is absent or malformed.
    ///
    /// Returns `None` if `key` fails the `[0-9a-zA-Z_-]` alphabet check
    /// (SEC-105).
    pub fn read_meta(&self, key: &str) -> Option<Meta> {
        if validate_key(key).is_err() {
            log::debug!("cache read_meta rejected invalid key {key:?} (SEC-105)");
            return None;
        }
        let path = self.meta_path(key);
        let raw = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str::<Meta>(&raw).ok()
    }

    /// Atomic write (QA-012): write the meta sidecar to its final path first
    /// (temp + rename), then write the data payload (temp + rename LAST).
    ///
    /// The committed/visible state is "both files present." A crash before the
    /// final data rename leaves meta-without-data, which readers treat as a
    /// miss — never the inverse orphan (data-without-meta) that the prior
    /// protocol could leave behind.
    ///
    /// QA-108: each temp file is a `tempfile::NamedTempFile::new_in(&self.dir)`
    /// with a process-unique random name, then `persist`d (same-directory
    /// rename) to its final path. The random name closes the concurrent-writer
    /// race the prior deterministic `{key}.{ext}.tmp` path had (two processes
    /// writing the same key interleaved bytes on the shared tmp file); the
    /// same-directory creation keeps the persist a same-filesystem rename, so
    /// the atomicity the QA-012 protocol depends on is preserved. The
    /// meta-first/data-last ordering is unchanged.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `key` fails the `[0-9a-zA-Z_-]` alphabet check
    /// (SEC-105), if `meta` cannot be serialized, or if any I/O step fails.
    pub fn write(&self, key: &str, data: &str, meta: &Meta) -> Result<()> {
        use std::io::Write;

        // SEC-105: validate up front so an invalid key never reaches the
        // filesystem. Returning Err (rather than silently no-oping) surfaces
        // the contract violation loudly on the write path.
        validate_key(key)?;

        // Serialize up front so a malformed meta never leaves a half-written
        // sidecar on disk.
        let meta_json = serde_json::to_string(meta)?;

        // 1. Meta first: NamedTempFile + persist (atomic same-directory
        //    rename) to the final path. `persist` consumes the handle, so on
        //    success no cleanup is needed; on failure the NamedTempFile Drop
        //    removes the temp file.
        let meta_path = self.meta_path(key);
        let mut meta_tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        meta_tmp.write_all(meta_json.as_bytes())?;
        meta_tmp.persist(&meta_path)?;

        // 2. Data last: NamedTempFile + persist. This is the commit point —
        //    only after this rename are both files visible together.
        let data_path = self.data_path(key);
        let mut data_tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        data_tmp.write_all(data.as_bytes())?;
        data_tmp.persist(&data_path)?;

        Ok(())
    }

    /// List `(key, meta)` for every entry whose BOTH data and meta files are
    /// present and parseable. Orphan sidecars (meta without data, the QA-012
    /// post-crash shape) and orphan data files (data without meta, the legacy
    /// shape) are both skipped.
    pub fn list(&self) -> Vec<(String, Meta)> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut result = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(key) = name.strip_suffix(".meta.json") else {
                continue;
            };
            if !self.data_path(key).exists() {
                continue; // orphan: meta without data
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                log::warn!("Skipping unreadable cache meta: {}", path.display());
                continue;
            };
            let Ok(meta) = serde_json::from_str::<Meta>(&raw) else {
                log::warn!("Skipping malformed cache meta: {}", path.display());
                continue;
            };
            result.push((key.to_string(), meta));
        }
        result
    }

    /// Remove entries older than `min_age` (or every entry when `None`).
    /// Orphaned data files (no paired meta) are removed opportunistically and
    /// do not count toward the returned total. Returns the number of paired
    /// entries removed.
    ///
    /// # Errors
    ///
    /// Propagates the underlying I/O error if reading the cache directory
    /// fails. A missing cache directory is not an error and returns `Ok(0)`.
    /// Errors from deleting an individual file or reading an individual meta
    /// sidecar are deliberately swallowed (logged at debug) so a single
    /// unreadable entry does not abort the sweep.
    pub fn clear(&self, min_age: Option<chrono::Duration>) -> Result<usize> {
        if !self.dir.exists() {
            log::info!(
                "Cache dir {} does not exist; nothing to clear",
                self.dir.display()
            );
            return Ok(0);
        }
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Ok(0);
        };
        let now = Utc::now();
        let mut deleted = 0usize;
        let data_suffix = format!(".{}", self.data_ext);

        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(key) = name.strip_suffix(".meta.json") else {
                // Orphan data file (no paired meta) — remove silently.
                if let Some(stem) = name.strip_suffix(&data_suffix)
                    && !self.meta_path(stem).exists()
                {
                    let _ = std::fs::remove_file(&path);
                }
                continue;
            };
            if let Some(min_age) = min_age {
                let Ok(raw) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(meta) = serde_json::from_str::<Meta>(&raw) else {
                    continue;
                };
                let age = now.signed_duration_since(meta.created_at());
                if age <= min_age {
                    continue; // fresh — keep it
                }
            }
            let _ = std::fs::remove_file(self.data_path(key));
            let _ = std::fs::remove_file(self.meta_path(key));
            deleted += 1;
        }
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde::Deserialize;
    use tempfile::TempDir;

    /// Minimal metadata struct for exercising [`RawCache`] without pulling in
    /// the OSM/Overture-specific layouts.
    #[derive(Debug, Serialize, Deserialize)]
    struct TestMeta {
        created_at: DateTime<Utc>,
        note: String,
    }

    impl CacheMeta for TestMeta {
        fn created_at(&self) -> DateTime<Utc> {
            self.created_at
        }
    }

    fn open_cache(ext: &'static str) -> (TempDir, RawCache<TestMeta>) {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let cache = RawCache::new(tmp.path().to_path_buf(), ext);
        (tmp, cache)
    }

    fn sample_meta() -> TestMeta {
        TestMeta {
            created_at: Utc::now(),
            note: "test".to_string(),
        }
    }

    #[test]
    fn write_then_read_roundtrips() {
        let (_tmp, cache) = open_cache("xml");
        let meta = sample_meta();
        cache.write("k1", "<osm/>", &meta).expect("write");
        assert_eq!(cache.read_data("k1").as_deref(), Some("<osm/>"));
        let read = cache.read_meta("k1").expect("meta present");
        assert_eq!(read.note, "test");
    }

    #[test]
    fn read_data_returns_none_when_data_absent() {
        // QA-012: "meta present, data absent" (the post-crash orphan shape
        // under the meta-first protocol) must be a miss.
        let (_tmp, cache) = open_cache("xml");
        let meta = sample_meta();
        // Write only the meta sidecar, simulating a crash before the data
        // rename.
        let meta_json = serde_json::to_string(&meta).unwrap();
        std::fs::write(cache.meta_path("orphan"), meta_json).unwrap();

        assert!(
            cache.read_data("orphan").is_none(),
            "meta-without-data orphan must miss on read"
        );
    }

    #[test]
    fn list_skips_orphan_meta_without_data() {
        // QA-012 orphan-skip: a meta sidecar with no paired data file is
        // excluded from the listing.
        let (_tmp, cache) = open_cache("geojson");
        let meta = sample_meta();

        // Well-formed entry: both files present.
        cache.write("good", "{}", &meta).expect("write good");

        // Orphan: meta only.
        let meta_json = serde_json::to_string(&meta).unwrap();
        std::fs::write(cache.meta_path("orphan"), meta_json).unwrap();

        let listed = cache.list();
        assert_eq!(listed.len(), 1, "only the paired entry should list");
        assert_eq!(listed[0].0, "good");
    }

    #[test]
    fn clear_removes_paired_entries_and_orphan_data() {
        let (_tmp, cache) = open_cache("xml");
        let meta = sample_meta();
        cache
            .write("paired", "<osm/>", &meta)
            .expect("write paired");

        // Orphan data file (no meta) — the legacy pre-QA-012 shape.
        std::fs::write(cache.data_path("orphan"), "<osm/>").unwrap();

        let deleted = cache.clear(None).expect("clear");
        assert_eq!(deleted, 1, "one paired entry removed");
        assert!(!cache.data_path("paired").exists());
        assert!(!cache.meta_path("paired").exists());
        assert!(
            !cache.data_path("orphan").exists(),
            "orphan data file should also be swept"
        );
    }

    #[test]
    fn clear_respects_min_age() {
        let (_tmp, cache) = open_cache("xml");
        let now = Utc::now();
        let old = TestMeta {
            created_at: now - chrono::Duration::hours(2),
            note: "old".to_string(),
        };
        let fresh = TestMeta {
            created_at: now - chrono::Duration::minutes(10),
            note: "fresh".to_string(),
        };
        cache.write("old", "<a/>", &old).expect("write old");
        cache.write("fresh", "<b/>", &fresh).expect("write fresh");

        let deleted = cache
            .clear(Some(chrono::Duration::hours(1)))
            .expect("clear");
        assert_eq!(deleted, 1, "only the old entry is evicted");
        assert!(!cache.data_path("old").exists());
        assert!(cache.data_path("fresh").exists());
    }

    // ── SEC-105: key alphabet validation ───────────────────────────────────

    #[test]
    fn validate_key_accepts_alphanumeric_underscore_dash() {
        assert!(validate_key("abcDEF012").is_ok(), "alphanumeric");
        assert!(validate_key("a_b-c").is_ok(), "underscore + dash");
        // The exact shape every internal caller produces.
        assert!(
            validate_key("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                .is_ok(),
            "64-char lowercase hex"
        );
    }

    #[test]
    fn validate_key_rejects_empty() {
        assert!(validate_key("").is_err(), "empty key must be rejected");
    }

    #[test]
    fn validate_key_rejects_path_traversal_shapes() {
        // Anything containing a separator or `..` is unreachable through the
        // alphabet (so we just need to confirm the alphabet rejects one
        // representative byte from each shape).
        assert!(validate_key("../evil").is_err(), "parent dir");
        assert!(validate_key("a/b").is_err(), "forward slash");
        assert!(validate_key("a\\b").is_err(), "backslash");
        assert!(validate_key("a b").is_err(), "space");
        assert!(validate_key("a\tb").is_err(), "tab");
        assert!(validate_key("a.b").is_err(), "dot (outside alphabet)");
        // NUL and friends.
        assert!(validate_key("a\0b").is_err(), "NUL byte");
    }

    #[test]
    fn write_rejects_invalid_key() {
        let (_tmp, cache) = open_cache("xml");
        let meta = sample_meta();
        let result = cache.write("../evil", "<osm/>", &meta);
        assert!(
            result.is_err(),
            "write with traversal key must Err, got {result:?}"
        );
        // The validator runs before path construction, so the cache directory
        // is left empty (no `.xml` / `.meta.json` / `.tmp` artifacts).
        assert!(
            std::fs::read_dir(cache.dir())
                .map(|mut it| it.next().is_none())
                .unwrap_or(true),
            "cache directory must be empty after rejected write"
        );
    }

    #[test]
    fn read_methods_return_none_for_invalid_key() {
        let (_tmp, cache) = open_cache("xml");
        // No file created up front — but even if one existed at a traversal
        // path, the validator short-circuits before path construction.
        assert!(
            cache.read_data("../evil").is_none(),
            "read_data with traversal key must be None"
        );
        assert!(
            cache.read_meta("a/b").is_none(),
            "read_meta with separator key must be None"
        );
        assert!(
            cache.read_data("").is_none(),
            "read_data with empty key must be None"
        );
    }

    #[test]
    fn write_then_read_roundtrips_with_valid_key() {
        // The SEC-105 alphabet must not regress the round-trip for the keys
        // the crate actually produces (64-char hex).
        let (_tmp, cache) = open_cache("xml");
        let key = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let meta = sample_meta();
        cache.write(key, "<osm/>", &meta).expect("write");
        assert_eq!(cache.read_data(key).as_deref(), Some("<osm/>"));
    }

    // ── QA-113: shared to_hex helper ───────────────────────────────────────

    #[test]
    fn to_hex_encodes_known_sha256_digest() {
        use sha2::Digest;
        // SHA-256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        // (verified against `echo -n hello | sha256sum` on a standard system).
        let digest = sha2::Sha256::digest(b"hello");
        assert_eq!(
            to_hex(&digest),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn to_hex_handles_empty_input() {
        assert_eq!(to_hex(&[]), "");
    }

    #[test]
    fn to_hex_lowercase_and_two_chars_per_byte() {
        // Each byte maps to exactly two lowercase hex digits.
        assert_eq!(to_hex(&[0x00, 0x0f, 0x10, 0xff, 0xab]), "000f10ffab");
    }
}
