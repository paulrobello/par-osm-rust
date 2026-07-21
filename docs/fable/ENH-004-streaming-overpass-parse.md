# ENH-004 — Streaming Overpass Fetch→Parse Pipeline

> Status: done (0.5.0, 2026-07-20) · Effort: Medium (~1 day) · Impact: ~50% peak-memory cut on large fetches
> **Hard prerequisite**: QA-102 (unified `BufRead` parse engine). Sequence after SEC-109
> (response cap) — this plan absorbs that cap into the streaming copy.

## Goal

Stop materializing the entire Overpass response body (hundreds of MB for large areas)
as an in-memory `String` before parsing. Stream the HTTP response to a cache-adjacent
temp file (bounded), then parse from disk with the streaming parser — peak memory
becomes roughly the parsed `OsmData` alone.

## Current State (verified at commit `55187f4`)

- `fetch_osm_xml` (`src/overpass.rs:250-273`) returns `Result<String>` via
  `res.text()` — the full-body buffer.
- `fetch_osm_data` (`src/overpass.rs:292-325`): cache read path returns the cached XML
  as a `String` (`osm_cache::read_for_url`) and parses with `parse_osm_xml_str`; miss
  path calls `fetch_osm_xml`, then `osm_cache::write_for_url(&key, bbox, filter, &xml,
  overpass_url)` (takes `&str`), then parses the string.
- `reqwest::blocking::Response` implements `std::io::Read`; the unified engine
  (post-QA-102) accepts any `BufRead`.
- The cache layer (`cache_store.rs` `RawCache::write`) takes byte slices; after QA-108
  it writes via `NamedTempFile` + persist.
- Cached entries are plain `.xml` files on disk whose paths the cache layer knows.

## Design Decision

**Stream-to-disk-then-parse** (not parse-while-teeing): the response streams into a
`NamedTempFile` in the cache directory with the SEC-109 byte cap enforced during the
copy; on success the temp file is persisted as the cache data file (meta written per
the existing meta-first protocol — read `RawCache::write` and mirror its ordering);
then `parse_osm_xml_file` parses from the cached path. One pass over the network, one
pass over the disk file; no full-body buffer ever exists. This also makes the cache
write crash-consistent for free and keeps `parse_osm_xml_str` out of the hot path.

## Implementation Steps

1. Read current `src/overpass.rs`, `src/osm_cache.rs`, `src/cache_store.rs` (post
   Phase-1/3 remediation — several of these were edited).
2. In `cache_store.rs`, add a streaming write:

   ```rust
   /// Stream `reader` into the cache as `key`'s data file (bounded by `max_bytes`),
   /// then write metadata. Mirrors `write`'s meta-first/data-last crash-consistency
   /// protocol — read that function and preserve the ordering + comments.
   pub(crate) fn write_from_reader<R: std::io::Read>(
       &self, key: &str, meta: &Meta, reader: R, max_bytes: u64,
   ) -> anyhow::Result<u64>   // returns bytes written
   ```

   Implementation: `validate_key` (SEC-105), `NamedTempFile` in the cache dir,
   `std::io::copy(&mut reader.take(max_bytes + 1), &mut tmp)?`, bail if written
   `> max_bytes`, then persist + meta exactly per the existing protocol ordering.
   ⚠️ Check the actual meta-first vs data-first order in the current `write` — the
   audit records "meta-first/data-last"; mirror whatever the code does, do not guess.
3. In `osm_cache.rs`, add `pub(crate) fn write_stream_for_url(key, bbox, filter,
   reader, overpass_url) -> Result<PathBuf>` wrapping `write_from_reader` and
   returning the persisted data-file path. Reuse the existing `CacheMeta`
   construction from `write_for_url`.
4. In `osm_cache.rs`, add `pub(crate) fn data_path_for(key) -> Option<PathBuf>`
   (or reuse an existing accessor if `RawCache` exposes one — check) so the read path
   can hand a *path* to the parser instead of a `String`.
5. Rewire `fetch_osm_data`:
   - Cache hit: get the path, `parse_osm_xml_file(&path)` instead of read-to-string +
     `parse_osm_xml_str`. (Keep the existing TTL/containment logic — containment hits
     also become path-based; read `find_containing_for_url` and add a path-returning
     variant beside it rather than changing its public signature.)
   - Miss: new internal `fetch_osm_response(bbox, filter, url) -> Result<reqwest::blocking::Response>`
     — the existing `fetch_osm_xml` minus body consumption (URL validation, query
     build, status/429 handling stay identical). Then
     `write_stream_for_url(...)` → `parse_osm_xml_file(&cached_path)`.
   - Cache-write failure: today a failed cache write only warns and the fetch still
     succeeds. Preserve that: on `write_stream_for_url` error, fall back to buffering
     (`res.text()` path, capped per SEC-109) so a full cache disk doesn't break fetches
     — or stream to a `tempfile::tempfile()` outside the cache and parse from it;
     pick the fallback, implement ONE of them, and document it.
6. Keep the public `fetch_osm_xml` (returns `String`) working unchanged — implement it
   over `fetch_osm_response` + capped `read_to_string` so there is exactly one request
   builder. It remains public API; only `fetch_osm_data` stops using it internally.
7. Tests:
   - Existing `fetch_osm_data` tests use injected fetchers at the `sources.rs` level —
     they should pass unchanged; run them.
   - `cache_store`: unit-test `write_from_reader` (content round-trips; over-cap reader
     is rejected and leaves no data file behind; meta/data ordering preserved — mirror
     the existing crash-consistency test shapes).
   - `osm_cache`: hit path returns a path whose contents parse; containment path ditto.
   - No live-network tests.
8. Rustdoc: update `fetch_osm_data`'s doc to describe the streaming behavior and the
   cap; add `# Errors` entries for the cap.

## Files to Touch

- `src/cache_store.rs` (`write_from_reader`)
- `src/osm_cache.rs` (stream write wrapper, path-based read/containment accessors)
- `src/overpass.rs` (`fetch_osm_response`, rewired `fetch_osm_data`, `fetch_osm_xml`
  reimplemented over the shared request path)
- Tests in all three modules

## Verification

```bash
cargo test overpass && cargo test osm_cache && cargo test cache_store && cargo test sources
make checkall
cargo bench --bench parse_osm_xml   # parser itself untouched — confirm no accidental drift
```

Manual (optional, network): one real small-bbox fetch; confirm cache file appears,
second call logs a cache hit, and `/usr/bin/time -l` peak RSS on a large bbox drops
versus the pre-change build.

## Rollback

The public API is unchanged; `fetch_osm_data` internals revert cleanly with the
commit. Cache format is untouched (same files, same meta), so mixed old/new versions
share the cache safely in both directions.
