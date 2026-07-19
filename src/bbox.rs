//! Shared bounding-box validation (SEC-102 / SEC-104).
//!
//! One validator for every public API that takes a `(south, west, north,
//! east)` bbox — keeps srtm and overpass from drifting apart. Private to the
//! crate; the public `BBox` newtype (ARC-106) is a deferred 0.3.0 change and
//! would live here when scheduled.

use anyhow::Result;

/// Validate a bounding box given in `(south, west, north, east)` decimal-degree
/// order.
///
/// Checks:
///   - every coordinate is finite (rejects `NaN` and `±∞` — all NaN
///     comparisons are false, so a plain `south >= north` cannot catch NaN),
///   - `south`/`north` are within `[-90, 90]`,
///   - `west`/`east` are within `[-180, 180]`,
///   - `south < north` and `west < east` (degenerate equal-bound bboxes are
///     caller error).
///
/// # Errors
///
/// Returns `Err` with a message naming the failed check on any violation.
pub(crate) fn validate_bbox(south: f64, west: f64, north: f64, east: f64) -> Result<()> {
    if !south.is_finite() || !west.is_finite() || !north.is_finite() || !east.is_finite() {
        anyhow::bail!("invalid bbox: non-finite coordinate ({south}, {west}, {north}, {east})");
    }
    if !(-90.0..=90.0).contains(&south) || !(-90.0..=90.0).contains(&north) {
        anyhow::bail!("invalid bbox: latitude out of [-90, 90] ({south}, {west}, {north}, {east})");
    }
    if !(-180.0..=180.0).contains(&west) || !(-180.0..=180.0).contains(&east) {
        anyhow::bail!(
            "invalid bbox: longitude out of [-180, 180] ({south}, {west}, {north}, {east})"
        );
    }
    if south >= north {
        anyhow::bail!("invalid bbox: south ({south}) must be < north ({north})");
    }
    if west >= east {
        anyhow::bail!("invalid bbox: west ({west}) must be < east ({east})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_bbox() {
        validate_bbox(-90.0, -180.0, 90.0, 180.0).expect("full-range bbox");
        validate_bbox(51.5, -0.13, 51.52, -0.10).expect("small bbox");
    }

    #[test]
    fn rejects_nan_in_any_position() {
        assert!(validate_bbox(f64::NAN, 0.0, 1.0, 1.0).is_err());
        assert!(validate_bbox(0.0, f64::NAN, 1.0, 1.0).is_err());
        assert!(validate_bbox(0.0, 0.0, f64::NAN, 1.0).is_err());
        assert!(validate_bbox(0.0, 0.0, 1.0, f64::NAN).is_err());
    }

    #[test]
    fn rejects_infinity() {
        assert!(validate_bbox(f64::INFINITY, 0.0, 1.0, 1.0).is_err());
        assert!(validate_bbox(0.0, f64::NEG_INFINITY, 1.0, 1.0).is_err());
    }

    #[test]
    fn rejects_out_of_range_latitude() {
        assert!(validate_bbox(-91.0, 0.0, 10.0, 10.0).is_err());
        assert!(validate_bbox(0.0, 0.0, 91.0, 10.0).is_err());
    }

    #[test]
    fn rejects_out_of_range_longitude() {
        assert!(validate_bbox(0.0, -181.0, 10.0, 10.0).is_err());
        assert!(validate_bbox(0.0, 0.0, 10.0, 181.0).is_err());
    }

    #[test]
    fn rejects_inverted_bounds() {
        assert!(validate_bbox(10.0, 0.0, 0.0, 10.0).is_err());
        assert!(validate_bbox(0.0, 10.0, 10.0, 0.0).is_err());
        // Equal bounds are also caller error (strict <).
        assert!(validate_bbox(0.0, 0.0, 0.0, 0.0).is_err());
    }
}
