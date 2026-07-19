//! Overture theme types and Overture → OSM tag/category mapping.
//!
//! Pure (no I/O, no feature gates). Owned by [`super`] and re-exported at
//! `crate::overture::*` for downstream callers (ARC-007 / QA-009).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Overture Maps theme selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OvertureTheme {
    /// Building footprints and building metadata.
    Building,
    /// Transportation segments, normalized mostly as OSM-style roads.
    Transportation,
    /// Places and POIs, normalized into tagged POI nodes.
    Place,
    /// Base land, land-use, water, and tree features.
    Base,
    /// Address points, normalized into address nodes.
    Address,
}

impl OvertureTheme {
    /// Return all supported themes in a stable default order.
    pub fn all() -> Vec<Self> {
        vec![
            Self::Building,
            Self::Transportation,
            Self::Place,
            Self::Base,
            Self::Address,
        ]
    }

    /// Return the `overturemaps download --type` values used for this theme.
    pub fn cli_types(&self) -> Vec<&'static str> {
        match self {
            Self::Building => vec!["building"],
            Self::Transportation => vec!["segment"],
            Self::Place => vec!["place"],
            Self::Base => vec!["land", "land_use", "water"],
            Self::Address => vec!["address"],
        }
    }

    /// Parse a user-facing theme string, accepting singular/plural aliases.
    ///
    /// # Examples
    ///
    /// ```
    /// use par_osm_rust::overture::OvertureTheme;
    ///
    /// assert_eq!(
    ///     OvertureTheme::from_str_loose("Buildings"),
    ///     Some(OvertureTheme::Building)
    /// );
    /// assert_eq!(
    ///     OvertureTheme::from_str_loose("road"),
    ///     Some(OvertureTheme::Transportation)
    /// );
    /// assert!(OvertureTheme::from_str_loose("unknown").is_none());
    /// ```
    pub fn from_str_loose(s: &str) -> Option<Self> {
        let theme = s.to_lowercase();
        match theme.as_str() {
            "address" | "addresses" | "addr" => Some(Self::Address),
            _ => match theme.strip_suffix('s').unwrap_or(&theme) {
                "building" => Some(Self::Building),
                "transportation" | "transport" | "road" | "segment" => Some(Self::Transportation),
                "place" => Some(Self::Place),
                "base" | "land" | "land_use" | "landuse" | "water" => Some(Self::Base),
                _ => None,
            },
        }
    }
}

impl std::fmt::Display for OvertureTheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Building => write!(f, "building"),
            Self::Transportation => write!(f, "transportation"),
            Self::Place => write!(f, "place"),
            Self::Base => write!(f, "base"),
            Self::Address => write!(f, "address"),
        }
    }
}

/// Which data source wins when Overture and OSM both cover the same non-POI theme.
///
/// **Deprecated (ARC-102, never implemented; will be removed in 0.3.0).**
/// This enum, [`OvertureParams::priority`](super::OvertureParams::priority),
/// and the `source_options` priority parsers are public and parseable but
/// no code path consumes them — every merge unconditionally keeps both
/// sources' non-POI geometry (equivalent to [`ThemePriority::Both`]).
/// Setting `priority = { Building: Osm }` does not exclude Overture
/// buildings. The promised behavior was never shipped, so 0.2.2 deprecates
/// the API surface and 0.3.0 will remove it. If you need theme-priority
/// filtering, open an issue so it can be implemented rather than re-added
/// in its current shape.
#[deprecated(since = "0.2.2", note = "never implemented; will be removed in 0.3.0")]
#[allow(deprecated)] // derive expansions (Default, serde) reference the variants
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePriority {
    /// Prefer Overture features for this theme.
    Overture,
    /// Prefer OSM/Overpass features for this theme.
    Osm,
    /// Keep features from both sources.
    #[default]
    Both,
}

/// Map an Overture place category string to the appropriate OSM primary key.
fn map_place_category_to_osm_key(category: &str) -> &'static str {
    match category {
        "restaurant" | "cafe" | "bar" | "fast_food" | "food_and_drink" => "amenity",
        "supermarket" | "grocery" | "clothing" | "electronics" | "retail" => "shop",
        "hotel" | "motel" | "hostel" | "accommodation" => "tourism",
        "park" | "playground" | "sports_centre" | "stadium" | "recreation" => "leisure",
        _ => "amenity",
    }
}

/// Map Overture feature properties to OSM-style tags for the given theme.
pub(super) fn map_tags_for_theme(props: &Value, theme: OvertureTheme) -> HashMap<String, String> {
    let mut tags: HashMap<String, String> = HashMap::new();

    match theme {
        OvertureTheme::Building => {
            // class → building (default "yes")
            let class = props.get("class").and_then(|v| v.as_str()).unwrap_or("yes");
            tags.insert("building".into(), class.to_string());

            // height → building:height
            if let Some(h) = props.get("height").and_then(|v| v.as_f64()) {
                tags.insert("building:height".into(), h.to_string());
            }
            // num_floors → building:levels
            if let Some(f) = props.get("num_floors").and_then(|v| v.as_u64()) {
                tags.insert("building:levels".into(), f.to_string());
            }
        }

        OvertureTheme::Transportation => {
            // class → highway (default "unclassified")
            let class = props
                .get("class")
                .and_then(|v| v.as_str())
                .unwrap_or("unclassified");
            tags.insert("highway".into(), class.to_string());

            // names.primary → name
            if let Some(name) = props
                .get("names")
                .and_then(|n| n.get("primary"))
                .and_then(|v| v.as_str())
            {
                tags.insert("name".into(), name.to_string());
            }
            // road_surface → surface
            if let Some(surface) = props.get("road_surface").and_then(|v| v.as_str()) {
                tags.insert("surface".into(), surface.to_string());
            }
            // is_bridge → bridge=yes
            if props
                .get("is_bridge")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                tags.insert("bridge".into(), "yes".into());
            }
            // is_tunnel → tunnel=yes
            if props
                .get("is_tunnel")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                tags.insert("tunnel".into(), "yes".into());
            }
        }

        OvertureTheme::Place => {
            // categories.primary → amenity / shop / tourism / leisure
            if let Some(category) = props
                .get("categories")
                .and_then(|c| c.get("primary"))
                .and_then(|v| v.as_str())
            {
                let osm_key = map_place_category_to_osm_key(category);
                tags.insert(osm_key.into(), category.to_string());
            }
            // names.primary → name
            if let Some(name) = props
                .get("names")
                .and_then(|n| n.get("primary"))
                .and_then(|v| v.as_str())
            {
                tags.insert("name".into(), name.to_string());
            }
        }

        OvertureTheme::Base => {
            // Overture Base uses "subtype" and "class" to distinguish features.
            // We map them to the appropriate OSM keys.
            let subtype = props.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
            let class = props.get("class").and_then(|v| v.as_str()).unwrap_or("");

            // Water bodies
            if matches!(
                subtype,
                "water" | "lake" | "pond" | "reservoir" | "ocean" | "sea"
            ) {
                tags.insert("natural".into(), "water".into());
                if !subtype.is_empty() && subtype != "water" {
                    tags.insert("water".into(), subtype.to_string());
                }
            }
            // Waterways
            else if matches!(subtype, "river" | "stream" | "canal" | "drain" | "ditch") {
                tags.insert("waterway".into(), subtype.to_string());
            }
            // Land use — from class when subtype indicates land_use
            else if matches!(
                subtype,
                "forest"
                    | "farmland"
                    | "residential"
                    | "commercial"
                    | "industrial"
                    | "cemetery"
                    | "grass"
                    | "scrub"
                    | "farmyard"
            ) {
                tags.insert("landuse".into(), subtype.to_string());
            }
            // Natural land cover from class
            else if matches!(subtype, "land" | "")
                && matches!(
                    class,
                    "grass" | "scrub" | "heath" | "bare_rock" | "sand" | "beach"
                )
            {
                tags.insert("natural".into(), class.to_string());
            }
            // Leisure areas
            else if matches!(subtype, "park" | "garden" | "pitch" | "playground") {
                tags.insert("leisure".into(), subtype.to_string());
            }
            // Individual tree points
            else if subtype == "tree" {
                tags.insert("natural".into(), "tree".into());
            }
            // Fallback: try the class field
            else if !class.is_empty() {
                tags.insert("landuse".into(), class.to_string());
            }
        }

        OvertureTheme::Address => {
            // number → addr:housenumber
            if let Some(number) = props.get("number").and_then(|v| v.as_str()) {
                tags.insert("addr:housenumber".into(), number.to_string());
            }
            // street → addr:street
            if let Some(street) = props.get("street").and_then(|v| v.as_str()) {
                tags.insert("addr:street".into(), street.to_string());
            }
        }
    }

    tags
}
