//! Bounding-box newtype + shared validation (ARC-106 / SEC-102 / SEC-104).
//!
//! [`BBox`] is the crate-wide type for a WGS-84 bounding box. Every public
//! API that takes a bbox now takes `&BBox` (or `BBox` by copy) so the
//! validation contract is encoded in the type, not repeated at each call
//! site. The internal `validate_bbox` helper remains as the backing check
//! used by [`BBox::new`].
//!
//! Field order is `(south, west, north, east)` (SWNE) throughout the crate;
//! Overture's CLI uses `(west, south, east, north)` (WSEN), available via
//! [`BBox::wsen`].

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
/// Backs [`BBox::new`]. Kept as a `pub(crate)` free function so internal
/// callers (srtm, overpass) can re-run the check on raw tuples without
/// constructing a [`BBox`].
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

/// WGS-84 bounding box in `(south, west, north, east)` decimal-degree order.
///
/// Construct via [`BBox::new`] (validates) for untrusted input, or via
/// [`BBox::from_unchecked`] / `From<(f64, f64, f64, f64)>` for input whose
/// validity is already established (e.g. constants in tests, or values read
/// back from a validated source). `BBox` is `Copy` (32 bytes), so API
/// signatures take it by value unless they need to mutate.
///
/// # Examples
///
/// ```
/// use par_osm_rust::bbox::BBox;
///
/// let bbox = BBox::new(51.5, -0.13, 51.52, -0.10).expect("valid bbox");
/// assert_eq!(bbox.south, 51.5);
/// assert_eq!(bbox.west, -0.13);
///
/// // Tuple conversion is unchecked — use for already-validated constants.
/// let from_tuple = BBox::from((0.0, 0.0, 1.0, 1.0));
/// assert_eq!(from_tuple, BBox::new(0.0, 0.0, 1.0, 1.0).unwrap());
///
/// // Overture CLI ordering is WSEN.
/// assert_eq!(bbox.wsen(), (-0.13, 51.5, -0.10, 51.52));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BBox {
    /// Southern latitude bound (decimal degrees, WGS-84).
    pub south: f64,
    /// Western longitude bound (decimal degrees, WGS-84).
    pub west: f64,
    /// Northern latitude bound (decimal degrees, WGS-84).
    pub north: f64,
    /// Eastern longitude bound (decimal degrees, WGS-84).
    pub east: f64,
}

impl BBox {
    /// Construct a validated [`BBox`] from `(south, west, north, east)`.
    ///
    /// Runs [`validate_bbox`](crate::bbox) (non-finite / out-of-range /
    /// inverted-bound rejection) so the result is always a usable bbox.
    /// Prefer this entry point at public API boundaries that receive
    /// untrusted input.
    ///
    /// # Errors
    ///
    /// Returns `Err` if any coordinate is non-finite, latitude is outside
    /// `[-90, 90]`, longitude is outside `[-180, 180]`, `south >= north`, or
    /// `west >= east`.
    pub fn new(south: f64, west: f64, north: f64, east: f64) -> Result<Self> {
        validate_bbox(south, west, north, east)?;
        Ok(Self {
            south,
            west,
            north,
            east,
        })
    }

    /// Construct a [`BBox`] without validation. For use when the caller has
    /// already established validity (e.g. a `const` in tests, a value read
    /// from a validated [`BBox::new`] call, or deserialization).
    ///
    /// Misuse constructs a struct that may surprise downstream code (NaN
    /// propagation, inverted bounds). Prefer [`BBox::new`] whenever input
    /// origin is unclear.
    pub const fn from_unchecked(south: f64, west: f64, north: f64, east: f64) -> Self {
        Self {
            south,
            west,
            north,
            east,
        }
    }

    /// Return the bbox as a `(west, south, east, north)` tuple — the WSEN
    /// ordering the `overturemaps` CLI expects for its `--bbox` argument.
    /// The crate's internal ordering remains SWNE; this is the boundary
    /// adapter.
    pub fn wsen(&self) -> (f64, f64, f64, f64) {
        (self.west, self.south, self.east, self.north)
    }

    /// Return the bbox as a SWNE tuple — convenience for callers that
    /// still work in `(f64, f64, f64, f64)` (e.g. the legacy `OsmData::bounds`
    /// accessor and `clip_to_bbox`).
    pub fn swne(&self) -> (f64, f64, f64, f64) {
        (self.south, self.west, self.north, self.east)
    }
}

impl From<(f64, f64, f64, f64)> for BBox {
    /// Unchecked conversion from a `(south, west, north, east)` tuple.
    ///
    /// Mechanical migration path for downstream code that previously held a
    /// raw tuple: `BBox::from(tuple)` is always infallible. For untrusted
    /// input, use [`BBox::new`] (the validating constructor) or
    /// [`BBox::try_from`](TryFrom) instead.
    fn from(bbox: (f64, f64, f64, f64)) -> Self {
        Self {
            south: bbox.0,
            west: bbox.1,
            north: bbox.2,
            east: bbox.3,
        }
    }
}

impl From<BBox> for (f64, f64, f64, f64) {
    /// Convert to a SWNE tuple (the crate's internal ordering). For the
    /// Overture CLI's WSEN ordering use [`BBox::wsen`].
    fn from(bbox: BBox) -> (f64, f64, f64, f64) {
        bbox.swne()
    }
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

    // ── BBox newtype (ARC-106) ──────────────────────────────────────────────

    #[test]
    fn new_validates_and_constructs() {
        let bbox = BBox::new(51.5, -0.13, 51.52, -0.10).expect("valid bbox");
        assert_eq!(bbox.south, 51.5);
        assert_eq!(bbox.west, -0.13);
        assert_eq!(bbox.north, 51.52);
        assert_eq!(bbox.east, -0.10);
    }

    #[test]
    fn new_rejects_invalid_bbox() {
        assert!(BBox::new(f64::NAN, 0.0, 1.0, 1.0).is_err());
        assert!(BBox::new(10.0, 0.0, 0.0, 10.0).is_err()); // inverted
        assert!(BBox::new(0.0, 0.0, 100.0, 10.0).is_err()); // lat out of range
    }

    #[test]
    fn from_tuple_is_unchecked() {
        // `From` does NOT validate, so even a NaN-containing tuple is accepted.
        let unchecked = BBox::from((f64::NAN, 0.0, 1.0, 1.0));
        assert!(unchecked.south.is_nan());
    }

    #[test]
    fn new_is_the_validating_constructor() {
        // Use `BBox::new` (or `BBox::try_from` via the blanket impl is NOT
        // available because `From` already provides `Into` — std's blanket
        // `TryFrom<U> for T where U: Into<T>` would conflict). For untrusted
        // tuples, call `BBox::new` directly.
        assert!(BBox::new(f64::NAN, 0.0, 1.0, 1.0).is_err());
        let bbox = BBox::new(0.0, 0.0, 1.0, 1.0).expect("valid tuple");
        assert_eq!(bbox.south, 0.0);
    }

    #[test]
    fn wsen_returns_west_south_east_north_order() {
        let bbox = BBox::from_unchecked(10.0, 20.0, 30.0, 40.0);
        // SWNE input (10, 20, 30, 40) → WSEN output (20, 10, 40, 30).
        assert_eq!(bbox.wsen(), (20.0, 10.0, 40.0, 30.0));
    }

    #[test]
    fn swne_round_trips_through_from() {
        let bbox = BBox::new(1.0, 2.0, 3.0, 4.0).unwrap();
        let tuple: (f64, f64, f64, f64) = bbox.into();
        assert_eq!(tuple, (1.0, 2.0, 3.0, 4.0));
        let back = BBox::from(tuple);
        assert_eq!(back, bbox);
    }

    #[test]
    fn copy_clone_eq_serde_round_trip() {
        let bbox = BBox::new(1.0, 2.0, 3.0, 4.0).unwrap();
        let copied = bbox; // Copy
        assert_eq!(bbox, copied);
        let cloned = bbox; // Clone (uses Copy)
        assert_eq!(bbox, cloned);

        let json = serde_json::to_string(&bbox).unwrap();
        let back: BBox = serde_json::from_str(&json).unwrap();
        assert_eq!(back, bbox);
    }
}
