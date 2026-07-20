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
/// Backs the [`Key::new`] constructor. Kept as a `pub(crate)` free function
/// so internal callers can re-run the check on raw strings without
/// constructing a [`Key`].
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

/// Validated cache-key newtype (SEC-105, ARC-113-bound work — 0.3.0).
///
/// A `Key` is the type-safe boundary for every cache-entry path. The
/// SEC-105 alphabet (`[0-9a-zA-Z_-]`, non-empty) is enforced by
/// [`Key::new`]; once a `Key` exists, no runtime re-check is needed at any
/// call site that takes `&Key`. Internal helpers that produce SHA-256 hex
/// output use the crate-private `Key::from_sha256_hex` constructor to wrap
/// the already-valid result.
///
/// # Examples
///
/// ```
/// use par_osm_rust::cache_store::Key;
///
/// let key = Key::new("deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef")?;
/// assert_eq!(key.as_str(), "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef");
///
/// // Traversal-shaped strings fail the alphabet check.
/// assert!(Key::new("../evil").is_err());
/// assert!(Key::new("").is_err());
/// # Ok::<(), anyhow::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key(String);

impl Key {
    /// Construct a validated [`Key`] from a string slice.
    ///
    /// Runs the SEC-105 alphabet check (non-empty, `[0-9a-zA-Z_-]` only)
    /// so the result is always a path-safe cache key. At public API
    /// boundaries that receive untrusted input, prefer this constructor.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `s` is empty or contains any byte outside the
    /// alphabet (transitively rejects `/`, `\`, `..`, spaces, `NUL`).
    pub fn new(s: &str) -> Result<Self> {
        validate_key(s)?;
        Ok(Self(s.to_string()))
    }

    /// Wrap an already-validated SHA-256 hex digest into a [`Key`] without
    /// re-running the alphabet check.
    ///
    /// Internal use only — the crate's `cache_key*` / `overture_cache_key*`
    /// helpers produce lowercase-hex SHA-256 digests that trivially satisfy
    /// the SEC-105 alphabet, so wrapping their output via this constructor
    /// avoids a redundant scan. The debug assertion catches a future caller
    /// that hands in an unvalidated string.
    pub(crate) fn from_sha256_hex(hex: String) -> Self {
        debug_assert!(
            validate_key(&hex).is_ok(),
            "from_sha256_hex called with non-alphabet input: {hex:?}"
        );
        Self(hex)
    }

    /// Borrow the validated key string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
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

    fn data_path_str(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.{}", self.data_ext))
    }

    /// Absolute path of `key`'s data file (`<dir>/<key>.<ext>`).
    ///
    /// Public so a streaming caller can hand a *path* to a streaming parser
    /// instead of reading the cached payload into a `String` first (ENH-004:
    /// the Overpass fetch path parses the cached file directly).
    pub fn data_path(&self, key: &Key) -> PathBuf {
        self.data_path_str(key.as_str())
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
    /// SEC-105 (0.3.0): `key` arrives as the validated [`Key`] newtype, so
    /// the alphabet check is enforced at construction time and is not
    /// re-run here.
    pub fn read_data(&self, key: &Key) -> Option<String> {
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
    pub fn read_meta(&self, key: &Key) -> Option<Meta> {
        let path = self.meta_path(key.as_str());
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
    /// SEC-105 (0.3.0): `key` is the validated [`Key`] newtype; the alphabet
    /// check is enforced at construction time and is not re-run here.
    ///
    /// # Errors
    ///
    /// Returns `Err` if `meta` cannot be serialized or if any I/O step fails.
    pub fn write(&self, key: &Key, data: &str, meta: &Meta) -> Result<()> {
        use std::io::Write;

        // Serialize up front so a malformed meta never leaves a half-written
        // sidecar on disk.
        let meta_json = serde_json::to_string(meta)?;

        // 1. Meta first: NamedTempFile + persist (atomic same-directory
        //    rename) to the final path. `persist` consumes the handle, so on
        //    success no cleanup is needed; on failure the NamedTempFile Drop
        //    removes the temp file.
        let meta_path = self.meta_path(key.as_str());
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

    /// Stream `reader` into a fresh [`tempfile::NamedTempFile`] inside this
    /// cache's directory, bounding the copy at `max_bytes` (ENH-004).
    ///
    /// The temp file lives in the cache directory so a later
    /// [`commit_temp`](Self::commit_temp) promotes it to the data path via a
    /// cheap same-filesystem rename. The cap is enforced *during* the copy via
    /// `reader.take(max_bytes + 1)`: an oversized body is rejected without
    /// ever being buffered fully, and on rejection the temp file is dropped
    /// (removed) so no orphan leaks.
    ///
    /// Returns the temp handle and the number of bytes copied. The byte count
    /// lets the caller populate an accurate `size_bytes` in the metadata
    /// before committing.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the temp file cannot be created, if the copy hits an
    /// I/O error, or if the reader yields more than `max_bytes` bytes.
    pub fn stream_to_temp<R: std::io::Read>(
        &self,
        reader: R,
        max_bytes: u64,
    ) -> Result<(tempfile::NamedTempFile, u64)> {
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        // take(max + 1) caps how many bytes io::copy pulls; if the reader has
        // more than max_bytes we observe written > max_bytes and bail. The
        // saturating_add avoids overflow when a caller passes u64::MAX.
        let written = std::io::copy(&mut reader.take(max_bytes.saturating_add(1)), &mut tmp)?;
        if written > max_bytes {
            anyhow::bail!("streamed body exceeded {max_bytes} byte cap (read {written} bytes)");
        }
        Ok((tmp, written))
    }

    /// Promote an already-written body temp file into `key`'s cache entry
    /// (ENH-004), preserving the QA-012 meta-first / data-last commit order
    /// used by [`write`](Self::write).
    ///
    /// The metadata sidecar is staged and persisted first; only then is the
    /// supplied `data_tmp` renamed into its final data path. A reader that
    /// observes the directory mid-commit therefore never sees data-without-meta
    /// (the legacy orphan shape) — at worst meta-without-data, which readers
    /// treat as a miss.
    ///
    /// **Non-fatal cache write.** If the commit fails *after* the bounded body
    /// copy already succeeded, the temp file handle is handed back in the
    /// `Err` variant so the caller can still parse from it instead of losing
    /// the fetched data. This preserves the prior "a cache hiccup warns but
    /// does not break the fetch" contract under streaming, where the body no
    /// longer lives in memory.
    ///
    /// # Errors
    ///
    /// Returns `Err((data_tmp, error))` — with the surviving temp handle — if
    /// metadata serialization, the meta sidecar write, or the final data
    /// rename fails.
    pub fn commit_temp(
        &self,
        key: &Key,
        meta: &Meta,
        data_tmp: tempfile::NamedTempFile,
    ) -> std::result::Result<PathBuf, (tempfile::NamedTempFile, anyhow::Error)> {
        use std::io::Write;

        // 1. Meta first (NamedTempFile + persist), mirroring `write`.
        let meta_result: Result<()> = (|| {
            let meta_json = serde_json::to_string(meta)?;
            let mut meta_tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
            meta_tmp.write_all(meta_json.as_bytes())?;
            meta_tmp.persist(self.meta_path(key.as_str()))?;
            Ok(())
        })();
        if let Err(error) = meta_result {
            // Body temp untouched — hand it back so the caller can still parse.
            return Err((data_tmp, error));
        }

        // 2. Data last: rename the supplied temp into place (the commit point).
        //    On failure, recover the handle from PersistError so the caller can
        //    still parse from the temp file.
        let data_path = self.data_path(key);
        match data_tmp.persist(&data_path) {
            Ok(_) => Ok(data_path),
            Err(tempfile::PersistError { file, error }) => Err((file, error.into())),
        }
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
            if !self.data_path_str(key).exists() {
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
            let _ = std::fs::remove_file(self.data_path_str(key));
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
    use std::io::Cursor;
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
        let key = Key::new("k1").unwrap();
        cache.write(&key, "<osm/>", &meta).expect("write");
        assert_eq!(cache.read_data(&key).as_deref(), Some("<osm/>"));
        let read = cache.read_meta(&key).expect("meta present");
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

        let key = Key::new("orphan").unwrap();
        assert!(
            cache.read_data(&key).is_none(),
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
        let good = Key::new("good").unwrap();
        cache.write(&good, "{}", &meta).expect("write good");

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
        let paired = Key::new("paired").unwrap();
        cache.write(&paired, "<osm/>", &meta).expect("write paired");

        // Orphan data file (no meta) — the legacy pre-QA-012 shape.
        std::fs::write(cache.data_path_str("orphan"), "<osm/>").unwrap();

        let deleted = cache.clear(None).expect("clear");
        assert_eq!(deleted, 1, "one paired entry removed");
        assert!(!cache.data_path_str("paired").exists());
        assert!(!cache.meta_path("paired").exists());
        assert!(
            !cache.data_path_str("orphan").exists(),
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
        let old_key = Key::new("old").unwrap();
        let fresh_key = Key::new("fresh").unwrap();
        cache.write(&old_key, "<a/>", &old).expect("write old");
        cache
            .write(&fresh_key, "<b/>", &fresh)
            .expect("write fresh");

        let deleted = cache
            .clear(Some(chrono::Duration::hours(1)))
            .expect("clear");
        assert_eq!(deleted, 1, "only the old entry is evicted");
        assert!(!cache.data_path_str("old").exists());
        assert!(cache.data_path_str("fresh").exists());
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
    fn key_new_rejects_invalid_alphabet() {
        // SEC-105 (0.3.0): the `Key` constructor is now the single point of
        // enforcement. The `RawCache` API no longer needs a runtime check;
        // an invalid key is unreachable past `Key::new`.
        assert!(Key::new("../evil").is_err(), "parent dir");
        assert!(Key::new("a/b").is_err(), "forward slash");
        assert!(Key::new("a\\b").is_err(), "backslash");
        assert!(Key::new("a b").is_err(), "space");
        assert!(Key::new("a\tb").is_err(), "tab");
        assert!(Key::new("a.b").is_err(), "dot (outside alphabet)");
        assert!(Key::new("a\0b").is_err(), "NUL byte");
        assert!(Key::new("").is_err(), "empty");
    }

    #[test]
    fn key_new_accepts_alphanumeric_underscore_dash() {
        assert!(Key::new("abcDEF012").is_ok(), "alphanumeric");
        assert!(Key::new("a_b-c").is_ok(), "underscore + dash");
        // The exact shape every internal caller produces.
        assert!(
            Key::new("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef").is_ok(),
            "64-char lowercase hex"
        );
    }

    #[test]
    fn key_from_sha256_hex_skips_redundant_validation() {
        // pub(crate) constructor wrapping known-good hex output.
        let k = Key::from_sha256_hex(
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
        );
        assert_eq!(
            k.as_str(),
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
        );
    }

    #[test]
    fn write_then_read_roundtrips_with_valid_key() {
        // The SEC-105 alphabet must not regress the round-trip for the keys
        // the crate actually produces (64-char hex).
        let (_tmp, cache) = open_cache("xml");
        let key = Key::new("deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef")
            .expect("valid 64-char hex key");
        let meta = sample_meta();
        cache.write(&key, "<osm/>", &meta).expect("write");
        assert_eq!(cache.read_data(&key).as_deref(), Some("<osm/>"));
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

    // ── ENH-004: streaming write primitives ───────────────────────────────
    //
    // `stream_to_temp` + `commit_temp` split the streaming write so the body
    // can be captured bounded onto disk (one network read) and then promoted
    // into the cache best-effort. See `RawCache::stream_to_temp` /
    // `RawCache::commit_temp`.

    #[test]
    fn data_path_joins_dir_key_and_ext() {
        let (tmp, cache) = open_cache("xml");
        let key = Key::new("deadbeef").unwrap();
        assert_eq!(
            cache.data_path(&key),
            tmp.path().join("deadbeef.xml"),
            "data_path must be <dir>/<key>.<ext>"
        );
    }

    #[test]
    fn stream_to_temp_copies_content_under_cap_and_returns_bytes() {
        let (_tmp, cache) = open_cache("xml");
        let payload = b"<osm><node id='1'/></osm>";
        let (file, written) = cache
            .stream_to_temp(Cursor::new(payload.to_vec()), 1024)
            .expect("under-cap stream succeeds");
        assert_eq!(written as usize, payload.len(), "bytes written reported");
        let on_disk = std::fs::read(file.path()).expect("temp file readable");
        assert_eq!(on_disk, payload, "streamed bytes round-trip");
    }

    #[test]
    fn stream_to_temp_accepts_reader_exactly_at_cap() {
        // take(max + 1) admits exactly max_bytes; the boundary must pass.
        let (_tmp, cache) = open_cache("xml");
        let payload = vec![b'x'; 64];
        let (_file, written) = cache
            .stream_to_temp(Cursor::new(payload), 64)
            .expect("exactly-at-cap must succeed");
        assert_eq!(written, 64);
    }

    #[test]
    fn stream_to_temp_rejects_reader_over_cap_and_leaves_no_temp() {
        let (tmp, cache) = open_cache("xml");
        let payload = vec![b'x'; 65];
        let err = cache
            .stream_to_temp(Cursor::new(payload), 64)
            .expect_err("over-cap reader must be rejected");
        assert!(
            err.to_string().contains("64"),
            "over-cap error should name the cap: {err}"
        );
        // The NamedTempFile is dropped on the error path, so the cache dir is
        // left empty — no orphan temp file leaks.
        let leftover = std::fs::read_dir(tmp.path())
            .map(|it| it.count())
            .unwrap_or(0);
        assert_eq!(leftover, 0, "no temp file should remain after over-cap");
    }

    #[test]
    fn commit_temp_persists_data_and_meta_round_trip() {
        let (_tmp, cache) = open_cache("xml");
        let key = Key::new("roundtrip123").unwrap();
        let payload = b"<osm/>";
        let (data_tmp, _bytes) = cache
            .stream_to_temp(Cursor::new(payload.to_vec()), 1024)
            .unwrap();
        let meta = sample_meta();
        let committed = cache
            .commit_temp(&key, &meta, data_tmp)
            .expect("commit succeeds");
        assert_eq!(
            committed,
            cache.data_path(&key),
            "returns the committed data path"
        );
        assert_eq!(
            cache.read_data(&key).as_deref(),
            Some("<osm/>"),
            "data round-trips after commit"
        );
        let read = cache.read_meta(&key).expect("meta present");
        assert_eq!(read.note, "test");
    }

    /// Meta type whose `Serialize` impl always fails — used to drive the
    /// commit-failure path of [`RawCache::commit_temp`] and confirm the
    /// bounded body is handed back instead of being dropped.
    #[derive(Debug)]
    struct AlwaysFailMeta;
    impl Serialize for AlwaysFailMeta {
        fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("test-injected serialize failure"))
        }
    }
    impl<'de> serde::Deserialize<'de> for AlwaysFailMeta {
        fn deserialize<D: serde::Deserializer<'de>>(_d: D) -> Result<Self, D::Error> {
            Ok(AlwaysFailMeta)
        }
    }
    impl CacheMeta for AlwaysFailMeta {
        fn created_at(&self) -> DateTime<Utc> {
            Utc::now()
        }
    }

    #[test]
    fn commit_temp_returns_surviving_temp_when_meta_write_fails() {
        // The non-fatal-cache-write contract: if the commit step fails AFTER
        // the bounded body copy succeeded, the caller must get the temp file
        // handle back so the fetch can still parse from it. A meta that fails
        // to serialize triggers that path inside commit_temp.
        let tmp = tempfile::tempdir().unwrap();
        let cache = RawCache::<AlwaysFailMeta>::new(tmp.path().to_path_buf(), "xml");
        let key = Key::new("failedbody1").unwrap();
        let payload = b"<osm><node id='1'/></osm>";
        let (data_tmp, _) = cache
            .stream_to_temp(Cursor::new(payload.to_vec()), 1024)
            .unwrap();

        let (surviving_tmp, err) = cache
            .commit_temp(&key, &AlwaysFailMeta, data_tmp)
            .expect_err("meta serialize failure must abort commit");
        assert!(
            err.to_string().contains("serialize"),
            "error should be the meta failure: {err}"
        );
        // The body is still intact in the surviving temp file.
        let on_disk = std::fs::read(surviving_tmp.path()).expect("surviving temp readable");
        assert_eq!(on_disk, payload, "body must survive a commit failure");
    }
}
