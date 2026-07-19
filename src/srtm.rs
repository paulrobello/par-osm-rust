//! Auto-download of SRTM elevation tiles.
//!
//! Fetches 1 arc-second HGT tiles from the AWS Terrain Tiles bucket
//! (Mapzen/Tilezen open data, no authentication required):
//!
//! ```text
//! https://s3.amazonaws.com/elevation-tiles-prod/skadi/{dir}/{name}.hgt.gz
//! ```
//!
//! Each tile is a gzip-compressed SRTM1 HGT file (3601 × 3601 × 2 bytes)
//! covering one 1°×1° cell, named after its SW corner: `N48W123.hgt.gz`.

use anyhow::Result;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::bbox::BBox;

const BASE_URL: &str = "https://s3.amazonaws.com/elevation-tiles-prod/skadi";

/// Hard cap on the number of 1°×1° SRTM tiles a single `tiles_for_bbox` call
/// may produce (SEC-102). A wild bbox (e.g. a saturating cast from `1e10`)
/// would otherwise push the loop into millions of iterations and trigger OOM
/// before any per-tile download cap could engage. 1000 tiles (~continental
/// scale) is far above any realistic single-fetch request and matches the
/// audit's recommendation.
pub const MAX_SRTM_TILES: usize = 1000;

// ── Cache directory ─────────────────────────────────────────────────────────

/// Return the persistent SRTM tile cache directory, creating it if needed.
pub fn cache_dir() -> PathBuf {
    crate::cache::srtm_cache_dir()
}

// ── Tile utilities ─────────────────────────────────────────────────────────

/// Return all 1°×1° tile SW corners needed to cover the input [`BBox`].
///
/// Each entry is `(lat_sw, lon_sw)` as signed integer degrees.
///
/// # Errors
///
/// Returns `Err` if the bbox fails validation (non-finite coordinate,
/// out-of-range latitude/longitude, inverted bounds) or if the resulting tile
/// count exceeds [`MAX_SRTM_TILES`] (SEC-102). Returning `Err` (rather than
/// silently clamping) is deliberate: clamping a wild bbox would download the
/// wrong tiles. ARC-106: bbox is the validated [`BBox`] newtype.
pub fn tiles_for_bbox(bbox: &BBox) -> Result<Vec<(i32, i32)>> {
    // SEC-102 / SEC-104: validate before any `as i32` cast — saturating casts
    // turn `1e10` into `i32::MAX`, so an unguarded wild bbox yields a loop of
    // up to ~2^64 iterations pushing tile pairs until OOM. ARC-106: bbox
    // arrives as the validated `BBox` newtype; validation runs once at
    // construction and is re-checked here as defense-in-depth.
    crate::bbox::validate_bbox(bbox.south, bbox.west, bbox.north, bbox.east)?;

    let lat0 = bbox.south.floor() as i32;
    let lat1 = bbox.north.ceil() as i32;
    let lon0 = bbox.west.floor() as i32;
    let lon1 = bbox.east.ceil() as i32;

    let mut tiles = Vec::new();
    for lat in lat0..lat1 {
        for lon in lon0..lon1 {
            tiles.push((lat, lon));
        }
    }

    if tiles.len() > MAX_SRTM_TILES {
        anyhow::bail!(
            "bbox ({}, {}, {}, {}) expands to {} SRTM tiles, \
             exceeding the {MAX_SRTM_TILES}-tile cap",
            bbox.south,
            bbox.west,
            bbox.north,
            bbox.east,
            tiles.len()
        );
    }

    Ok(tiles)
}

/// Format a tile SW corner `(lat_sw, lon_sw)` as the standard HGT filename
/// stem (without extension): e.g. `(48, -123)` → `"N48W123"`.
pub fn tile_name(lat: i32, lon: i32) -> String {
    let ns = if lat >= 0 { 'N' } else { 'S' };
    let ew = if lon >= 0 { 'E' } else { 'W' };
    format!("{ns}{:02}{ew}{:03}", lat.unsigned_abs(), lon.unsigned_abs())
}

// ── Download ───────────────────────────────────────────────────────────────

/// Lazily-initialized shared `reqwest::blocking::Client` for SRTM tile downloads.
///
/// Reused across calls to enable HTTP connection pooling — downloading many
/// tiles from the same S3 host reuses the underlying TCP/TLS connection
/// instead of paying setup cost on every call (ARC-020).
///
/// Configuration preserved on the pooled client:
///   - `redirect(Policy::none())` — defense-in-depth (SEC-003): the URL is a
///     hardcoded const + integer-derived tile name (bounded), but disabling
///     redirects prevents any future change to the URL scheme from silently
///     enabling redirect-based attacks, and matches the Overpass client's
///     posture.
///   - 120 s timeout (SEC-007) — SRTM1 tiles are ~25 MB; allow generous time
///     for slow connections so we don't indefinitely block the Tokio thread
///     pool.
///
/// Built with `OnceLock::get_or_init` so the first successful build is reused
/// for the process lifetime; a build failure propagates as an `anyhow::Error`
/// via `?` instead of panicking. (`OnceLock::get_or_try_init` is unstable on
/// stable Rust — see issue #109737 — so we check-then-init manually. Two
/// racing callers may each build a client; the loser's is discarded.)
fn shared_client() -> Result<&'static reqwest::blocking::Client> {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c);
    }
    let c = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build HTTP client: {e}"))?;
    Ok(CLIENT.get_or_init(|| c))
}

/// Decompressed size of an SRTM1 1-arc-second tile: 3601 × 3601 × 2 bytes.
const SRTM1_HGT_BYTES: usize = 25_934_402;
/// Decompressed size of an SRTM3 3-arc-second tile: 1201 × 1201 × 2 bytes.
const SRTM3_HGT_BYTES: usize = 2_884_802;
/// Hard upper bound on a legal decompressed HGT payload (SRTM1 is the larger
/// of the two known shapes). SEC-101: a bomb whose decompressed size exceeds
/// this is rejected before it reaches the cache.
const MAX_HGT_BYTES: u64 = SRTM1_HGT_BYTES as u64;
/// Pre-decompression bound on the response body (SEC-101). The compressed
/// SRTM1 payload is well under 1 MB; 30 MB is generous against any plausible
/// mirror-side re-encoding while still ruling out gigabyte-scale bombs before
/// they hit `response.bytes()`.
const MAX_GZ_RESPONSE_BYTES: u64 = 30 * 1024 * 1024;

/// Gzip-decode and size-validate an SRTM `.hgt.gz` payload (SEC-101).
///
/// Decompression is bounded by [`MAX_HGT_BYTES`] (read at most `MAX + 1` so
/// an off-by-one on the exact legal size is detectable), then the
/// decompressed length is required to be exactly [`SRTM1_HGT_BYTES`] or
/// [`SRTM3_HGT_BYTES`] — anything else means the payload is corrupt or
/// malicious and must not reach the cache.
///
/// Extracted from [`download_tile`] so the bound is unit-testable without a
/// network round-trip.
///
/// # Errors
///
/// Returns `Err` if gzip decompression fails, the decompressed size exceeds
/// [`MAX_HGT_BYTES`], or the decompressed size is not one of the two legal
/// HGT sizes.
pub(crate) fn decode_hgt_gz(gz_bytes: &[u8]) -> Result<Vec<u8>> {
    // `.take(MAX + 1)` caps how many decompressed bytes are read: a legal
    // tile (≤ MAX) finishes cleanly; a bomb stops at MAX + 1 and is rejected
    // by the size check below without reading the whole stream.
    let mut decoder = flate2::read::GzDecoder::new(gz_bytes).take(MAX_HGT_BYTES + 1);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| anyhow::anyhow!("Gzip decompression failed: {e}"))?;

    if out.len() as u64 > MAX_HGT_BYTES {
        anyhow::bail!(
            "decompressed SRTM tile size {} B exceeds maximum {} B (decompression bomb?)",
            out.len(),
            MAX_HGT_BYTES
        );
    }

    if out.len() != SRTM1_HGT_BYTES && out.len() != SRTM3_HGT_BYTES {
        anyhow::bail!(
            "unexpected SRTM tile size {} B (legal sizes: SRTM1={SRTM1_HGT_BYTES}, SRTM3={SRTM3_HGT_BYTES})",
            out.len()
        );
    }

    Ok(out)
}

/// Download, decompress, and save a single SRTM tile to `dest_dir`.
///
/// Skips the download if the `.hgt` file already exists.
/// Returns `Ok(true)` if the tile was downloaded, `Ok(false)` if it already existed.
///
/// # Errors
///
/// Returns `Err` on HTTP failure, on a response whose announced `Content-Length`
/// exceeds `MAX_GZ_RESPONSE_BYTES` (SEC-101 pre-buffer check), on a payload
/// whose decompressed size exceeds `MAX_HGT_BYTES` or is not one of the two
/// legal HGT sizes, or on filesystem failure during the atomic write.
pub fn download_tile(lat: i32, lon: i32, dest_dir: &Path) -> Result<bool> {
    let name = tile_name(lat, lon);
    let hgt_path = dest_dir.join(format!("{name}.hgt"));

    if hgt_path.exists() {
        log::debug!("Elevation tile {name} already exists — skipping");
        return Ok(false);
    }

    // Build the directory component: e.g. "N48" or "S05"
    let ns = if lat >= 0 { 'N' } else { 'S' };
    let dir_part = format!("{ns}{:02}", lat.unsigned_abs());
    let url = format!("{BASE_URL}/{dir_part}/{name}.hgt.gz");

    log::info!("Downloading elevation tile {name}…");

    let client = shared_client()?;

    let response = client
        .get(&url)
        .send()
        .map_err(|e| anyhow::anyhow!("Request failed for {url}: {e}"))?;

    if !response.status().is_success() {
        anyhow::bail!("HTTP {} downloading {}", response.status(), url);
    }

    // SEC-101: reject oversized responses *before* buffering them into
    // memory. Content-Length is advisory (the mirror can lie or omit it), so
    // the decompressed-size bound in `decode_hgt_gz` is the load-bearing
    // guard; this pre-check just keeps a 10 GB response from reaching
    // `response.bytes()` at all.
    if let Some(len) = response.content_length()
        && len > MAX_GZ_RESPONSE_BYTES
    {
        anyhow::bail!(
            "SRTM response for {name} announces Content-Length {len} B > {MAX_GZ_RESPONSE_BYTES} B cap"
        );
    }

    let gz_bytes = response
        .bytes()
        .map_err(|e| anyhow::anyhow!("Failed to read response body for {name}: {e}"))?;

    let hgt_data = decode_hgt_gz(gz_bytes.as_ref())
        .map_err(|e| anyhow::anyhow!("Failed to decode SRTM tile {name}: {e}"))?;

    // SEC-108: write to a unique temp file via `tempfile::NamedTempFile`,
    // then `persist` (atomic same-directory rename) into the final path.
    // Compared to the prior fixed-name `{name}.hgt.tmp` + `fs::write`:
    //   - `NamedTempFile::new_in` uses `O_EXCL` + a random name, so two
    //     processes downloading the same tile cannot interleave writes on a
    //     shared tmp path (one would silently overwrite the other's bytes).
    //   - Random names defeat symlink pre-planting: an attacker who can write
    //     `dest_dir` cannot predict the temp path to lure the write into
    //     following a planted symlink.
    //   - `persist` is a rename within the same filesystem (we explicitly
    //     create the temp in `dest_dir`), preserving the atomicity the
    //     mmap SAFETY argument in `HgtTile::load` depends on.
    let mut tmp = tempfile::NamedTempFile::new_in(dest_dir).map_err(|e| {
        anyhow::anyhow!("Failed to create temp file in {}: {e}", dest_dir.display())
    })?;
    tmp.write_all(&hgt_data)
        .map_err(|e| anyhow::anyhow!("Failed to write temp file {}: {e}", tmp.path().display()))?;
    tmp.persist(&hgt_path).map_err(|e| {
        anyhow::anyhow!(
            "Failed to persist temp file {} → {}: {e}",
            e.file.path().display(),
            hgt_path.display()
        )
    })?;

    log::info!(
        "Saved elevation tile {} ({:.1} MB)",
        name,
        hgt_data.len() as f64 / 1_048_576.0
    );
    Ok(true)
}

/// Download a single tile, retrying up to `max_retries` times on failure.
///
/// Returns `Ok(true)` if downloaded, `Ok(false)` if already cached, or an
/// error describing all attempts if every try failed.
fn download_tile_with_retry(lat: i32, lon: i32, dest_dir: &Path, max_retries: u32) -> Result<bool> {
    let name = tile_name(lat, lon);
    let mut last_err = anyhow::anyhow!("no attempts made");
    for attempt in 1..=max_retries {
        match download_tile(lat, lon, dest_dir) {
            Ok(v) => return Ok(v),
            Err(e) => {
                last_err = e;
                if attempt < max_retries {
                    let delay = std::time::Duration::from_secs(2u64.pow(attempt - 1));
                    log::warn!(
                        "Elevation tile {name} attempt {attempt}/{max_retries} failed: {last_err} — retrying in {}s",
                        delay.as_secs()
                    );
                    std::thread::sleep(delay);
                }
            }
        }
    }
    Err(last_err.context(format!(
        "elevation tile {name} failed after {max_retries} attempts"
    )))
}

/// Download all SRTM tiles needed to cover the given bounding box into
/// `dest_dir`.
///
/// ARC-108 (0.3.0): `progress_cb` now follows the crate-wide `ProgressFn`
/// contract — a fraction in `0.0..=1.0` and a human-readable message. The
/// prior raw `(tile_index, total_tiles, tile_name)` callback is mapped
/// internally: `done/total → fraction` and `"SRTM tile {name} ({i+1}/{total})"`
/// into the message string. The fraction is monotonic (clamped + guarded by
/// the shared `emit_progress` helper).
///
/// Each tile is retried up to 3 times with exponential backoff (1 s, 2 s).
/// If any tile fails all retries the function returns an error — the caller
/// should abort the conversion rather than silently produce flat terrain.
///
/// # Errors
///
/// Propagates any [`tiles_for_bbox`] validation error (non-finite /
/// out-of-range / inverted bbox, tile count over [`MAX_SRTM_TILES`]), and
/// bails if any tile fails all retries.
///
/// Returns the number of tiles actually downloaded (excludes pre-existing ones).
pub fn download_tiles_for_bbox(
    bbox: &BBox,
    dest_dir: &Path,
    progress_cb: crate::ProgressFn<'_>,
) -> Result<usize> {
    let tiles = tiles_for_bbox(bbox)?;
    let total = tiles.len();

    if total == 0 {
        log::warn!("No SRTM tiles computed for bbox — bbox may be empty");
        return Ok(0);
    }

    log::info!("Downloading {total} SRTM tile(s) for bounding box");

    let mut downloaded = 0usize;
    let mut failed: Vec<String> = Vec::new();
    let mut last_progress = 0.0f32;

    for (i, (lat, lon)) in tiles.iter().enumerate() {
        let name = tile_name(*lat, *lon);
        // ARC-108: map raw counts to the shared ProgressFn contract.
        let fraction = i as f32 / total as f32;
        let message = format!("SRTM tile {name} ({}/{total})", i + 1);
        crate::emit_progress(progress_cb, &mut last_progress, fraction, &message);
        match download_tile_with_retry(*lat, *lon, dest_dir, 3) {
            Ok(true) => downloaded += 1,
            Ok(false) => {}
            Err(e) => {
                log::error!("Elevation tile {name} could not be downloaded: {e:#}");
                failed.push(name);
            }
        }
    }

    if !failed.is_empty() {
        anyhow::bail!(
            "Failed to download {} elevation tile(s): {}. \
             Cannot generate terrain without complete elevation data.",
            failed.len(),
            failed.join(", ")
        );
    }

    // ARC-108: pin progress to 1.0 on success so callers observe completion.
    crate::emit_progress(progress_cb, &mut last_progress, 1.0, "SRTM tiles ready");
    log::info!("Elevation tiles ready ({downloaded} new, {total} total)");
    Ok(downloaded)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_name_north_west() {
        assert_eq!(tile_name(48, -123), "N48W123");
    }

    #[test]
    fn tile_name_south_east() {
        assert_eq!(tile_name(-33, 151), "S33E151");
    }

    #[test]
    fn tile_name_equator_prime_meridian() {
        assert_eq!(tile_name(0, 0), "N00E000");
    }

    #[test]
    fn tiles_for_bbox_single_tile() {
        // A small bbox well within one degree cell
        let tiles = tiles_for_bbox(&BBox::from((48.1, -122.9, 48.8, -122.1))).expect("valid bbox");
        assert_eq!(tiles.len(), 1);
        assert!(tiles.contains(&(48, -123)));
    }

    #[test]
    fn tiles_for_bbox_two_columns() {
        // Spans the lon=-123 boundary
        let tiles = tiles_for_bbox(&BBox::from((48.1, -123.5, 48.8, -122.5))).expect("valid bbox");
        assert_eq!(tiles.len(), 2);
        assert!(tiles.contains(&(48, -124)));
        assert!(tiles.contains(&(48, -123)));
    }

    #[test]
    fn tiles_for_bbox_four_tiles() {
        // Spans both a lat and a lon boundary
        let tiles = tiles_for_bbox(&BBox::from((47.5, -123.5, 48.5, -122.5))).expect("valid bbox");
        assert_eq!(tiles.len(), 4);
    }

    #[test]
    fn tiles_for_empty_bbox() {
        // Equal bounds produce zero tiles legitimately — but SEC-102's
        // validation rejects `min >= max`, so the degenerate case now returns
        // Err. Confirm the validator rejects it rather than producing an
        // empty Vec silently.
        let result = tiles_for_bbox(&BBox::from_unchecked(0.0, 0.0, 0.0, 0.0));
        assert!(
            result.is_err(),
            "degenerate equal-bound bbox must error, got {result:?}"
        );
    }

    // ── SEC-102 validation ─────────────────────────────────────────────────

    #[test]
    fn tiles_for_bbox_rejects_nan_in_any_position() {
        // NaN comparisons are false, so without an explicit is_finite() check
        // the floor/ceil casts would happily produce garbage tile ranges.
        // ARC-106: invalid bboxes must be built via `from_unchecked` because
        // `BBox::new` would reject them at construction time.
        assert!(tiles_for_bbox(&BBox::from_unchecked(f64::NAN, 0.0, 1.0, 1.0)).is_err());
        assert!(tiles_for_bbox(&BBox::from_unchecked(0.0, f64::NAN, 1.0, 1.0)).is_err());
        assert!(tiles_for_bbox(&BBox::from_unchecked(0.0, 0.0, f64::NAN, 1.0)).is_err());
        assert!(tiles_for_bbox(&BBox::from_unchecked(0.0, 0.0, 1.0, f64::NAN)).is_err());
    }

    #[test]
    fn tiles_for_bbox_rejects_infinity() {
        assert!(tiles_for_bbox(&BBox::from_unchecked(f64::INFINITY, 0.0, 1.0, 1.0)).is_err());
        assert!(tiles_for_bbox(&BBox::from_unchecked(0.0, 0.0, 1.0, f64::NEG_INFINITY)).is_err());
    }

    #[test]
    fn tiles_for_bbox_rejects_out_of_range_latitude() {
        // ±91 must be rejected; the legal range is [-90, 90].
        assert!(tiles_for_bbox(&BBox::from_unchecked(-91.0, 0.0, 10.0, 10.0)).is_err());
        assert!(tiles_for_bbox(&BBox::from_unchecked(0.0, 0.0, 91.0, 10.0)).is_err());
        // Boundary values ±90 are accepted — use a tiny bbox so the tile
        // count stays well under MAX_SRTM_TILES.
        tiles_for_bbox(&BBox::new(-90.0, 0.0, -89.5, 0.5).unwrap())
            .expect("-90 latitude boundary accepted");
        tiles_for_bbox(&BBox::new(89.5, 0.0, 90.0, 0.5).unwrap())
            .expect("+90 latitude boundary accepted");
    }

    #[test]
    fn tiles_for_bbox_rejects_out_of_range_longitude() {
        // ±181 must be rejected; the legal range is [-180, 180].
        assert!(tiles_for_bbox(&BBox::from_unchecked(0.0, -181.0, 10.0, 10.0)).is_err());
        assert!(tiles_for_bbox(&BBox::from_unchecked(0.0, 0.0, 10.0, 181.0)).is_err());
    }

    #[test]
    fn tiles_for_bbox_rejects_inverted_bounds() {
        // min_lat >= max_lat or min_lon >= max_lon is caller error.
        assert!(tiles_for_bbox(&BBox::from_unchecked(10.0, 0.0, 0.0, 10.0)).is_err());
        assert!(tiles_for_bbox(&BBox::from_unchecked(0.0, 10.0, 10.0, 0.0)).is_err());
    }

    #[test]
    fn tiles_for_bbox_enforces_tile_count_cap() {
        // Just under the cap: full-globe latitude span × a tight longitude
        // span. Realistic-sized bboxes never approach the cap.
        let near = tiles_for_bbox(&BBox::from((-89.5, 0.0, 89.5, 0.5))).expect("near-cap bbox");
        assert!(near.len() <= MAX_SRTM_TILES);

        // Over the cap: a bbox deliberately sized to exceed MAX_SRTM_TILES.
        // MAX_SRTM_TILES = 1000; an 1800-tile span (60 lat × 30 lon) is
        // well over.
        let over = tiles_for_bbox(&BBox::from((-30.0, 0.0, 30.0, 30.0)));
        assert!(
            over.is_err(),
            "over-cap bbox must error ({} tiles requested)",
            60 * 30
        );
        let msg = over.unwrap_err().to_string();
        assert!(
            msg.contains("exceeding") && msg.contains("tile cap"),
            "error should mention the cap: {msg}"
        );
    }

    // ── SEC-101: gzip decompression bound ──────────────────────────────────

    /// Helper: gzip a byte slice into a fresh `Vec<u8>`.
    fn gzip(bytes: &[u8]) -> Vec<u8> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(bytes).expect("encode");
        enc.finish().expect("finish")
    }

    #[test]
    fn decode_hgt_gz_rejects_decompression_bomb() {
        // 26 MB of zeros: one byte over MAX_HGT_BYTES (SRTM1 size). The
        // bounded read stops at MAX + 1 and the size check rejects it
        // instead of materializing the whole bomb.
        let oversized = vec![0u8; (MAX_HGT_BYTES + 1) as usize];
        let gz = gzip(&oversized);
        let result = decode_hgt_gz(&gz);
        assert!(result.is_err(), "oversized payload must be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("exceeds maximum"),
            "error should mention the cap: {msg}"
        );
    }

    #[test]
    fn decode_hgt_gz_rejects_wrong_legal_size() {
        // A 1000-byte payload decompresses cleanly but is neither SRTM1 nor
        // SRTM3, so it must be rejected before reaching the cache.
        let bogus = vec![0u8; 1000];
        let gz = gzip(&bogus);
        let result = decode_hgt_gz(&gz);
        assert!(result.is_err(), "non-legal HGT size must be rejected");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("unexpected SRTM tile size"),
            "error should mention unexpected size: {msg}"
        );
    }

    #[test]
    fn decode_hgt_gz_accepts_srtm3_sized_payload() {
        // SRTM3 (1201×1201×2 = 2_884_802 B) is the smaller of the two legal
        // sizes; an exact-size payload must decode cleanly.
        let payload = vec![0u8; SRTM3_HGT_BYTES];
        let gz = gzip(&payload);
        let out = decode_hgt_gz(&gz).expect("legal SRTM3 size accepted");
        assert_eq!(out.len(), SRTM3_HGT_BYTES);
    }

    #[test]
    fn decode_hgt_gz_accepts_srtm1_sized_payload() {
        // SRTM1 (3601×3601×2 = 25_934_402 B) is the larger legal size and
        // equals MAX_HGT_BYTES; an exact-MAX payload must still pass (the
        // bound is exclusive at MAX + 1, inclusive at MAX).
        let payload = vec![0u8; SRTM1_HGT_BYTES];
        let gz = gzip(&payload);
        let out = decode_hgt_gz(&gz).expect("legal SRTM1 size accepted");
        assert_eq!(out.len(), SRTM1_HGT_BYTES);
    }
}
