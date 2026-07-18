//! Feature filter controlling which OSM feature types are included in conversion.

use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

/// Controls which OSM feature categories are fetched from Overpass and/or
/// rendered during world generation. All fields default to `true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeatureFilter {
    /// Include highways and other routable ways (`highway=*`).
    #[serde(default = "default_true")]
    pub roads: bool,
    /// Include building footprints and entrance/address nodes (`building=*`,
    /// `addr:housenumber=*`).
    #[serde(default = "default_true")]
    pub buildings: bool,
    /// Include waterways and water bodies (`waterway=*`, `natural=water`).
    #[serde(default = "default_true")]
    pub water: bool,
    /// Include landuse polygons and natural areas (`landuse=*`, `natural=*`).
    #[serde(default = "default_true")]
    pub landuse: bool,
    /// Include rail lines (`railway=rail`).
    #[serde(default = "default_true")]
    pub railways: bool,
}

impl Default for FeatureFilter {
    fn default() -> Self {
        Self {
            roads: true,
            buildings: true,
            water: true,
            landuse: true,
            railways: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enables_all() {
        let f = FeatureFilter::default();
        assert!(f.roads, "roads should be enabled by default");
        assert!(f.buildings, "buildings should be enabled by default");
        assert!(f.water, "water should be enabled by default");
        assert!(f.landuse, "landuse should be enabled by default");
        assert!(f.railways, "railways should be enabled by default");
    }

    #[test]
    fn serde_empty_json_uses_defaults() {
        let f: FeatureFilter = serde_json::from_str("{}").unwrap();
        assert!(f.roads && f.buildings && f.water && f.landuse && f.railways);
    }

    #[test]
    fn serde_partial_disable() {
        let f: FeatureFilter = serde_json::from_str(r#"{"roads":false,"landuse":false}"#).unwrap();
        assert!(!f.roads, "roads should be disabled");
        assert!(!f.landuse, "landuse should be disabled");
        assert!(f.buildings, "buildings should still be enabled");
        assert!(f.water, "water should still be enabled");
        assert!(f.railways, "railways should still be enabled");
    }
}
