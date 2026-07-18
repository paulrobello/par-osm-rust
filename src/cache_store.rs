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

/// Marker trait for a cache entry's metadata sidecar.
///
/// Binds [`Serialize`] + [`DeserializeOwned`] (so the sidecar can be written
/// and read by [`RawCache`]) and exposes the entry's written-at timestamp,
/// which [`RawCache::clear`] uses for age-based eviction.
pub trait CacheMeta: Serialize + DeserializeOwned {
    /// Return the UTC timestamp at which this entry was written.
    fn created_at(&self) -> DateTime<Utc>;
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

    fn data_tmp(&self, key: &str) -> PathBuf {
        self.dir
            .join(format!("{key}.{ext}.tmp", ext = self.data_ext))
    }

    fn meta_tmp(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.meta.json.tmp"))
    }

    /// Read the data payload for `key`, or `None` if the data file is absent
    /// or unreadable.
    ///
    /// Per the QA-012 protocol, "meta present, data absent" (the post-crash
    /// orphan shape) is naturally a miss here: the data file does not exist,
    /// so this returns `None`. Callers that also need a meta-sidecar check
    /// (e.g. URL or TTL matching) should pair this with [`read_meta`](Self::read_meta)
    /// and apply the check themselves before consuming the payload.
    pub fn read_data(&self, key: &str) -> Option<String> {
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
    pub fn read_meta(&self, key: &str) -> Option<Meta> {
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
    pub fn write(&self, key: &str, data: &str, meta: &Meta) -> Result<()> {
        // Serialize up front so a malformed meta never leaves a half-written
        // sidecar on disk.
        let meta_json = serde_json::to_string(meta)?;

        // 1. Meta first: temp + rename to final path.
        let meta_tmp = self.meta_tmp(key);
        let meta_path = self.meta_path(key);
        std::fs::write(&meta_tmp, &meta_json)?;
        std::fs::rename(&meta_tmp, &meta_path)?;

        // 2. Data last: temp + rename to final path. This is the commit point
        //    — only after this rename are both files visible together.
        let data_tmp = self.data_tmp(key);
        let data_path = self.data_path(key);
        std::fs::write(&data_tmp, data)?;
        std::fs::rename(&data_tmp, &data_path)?;

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
}
