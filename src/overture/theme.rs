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

/// One declarative Overture → OSM tag mapping.
///
/// Each variant names where to read the source value in `props` and which
/// OSM tag key to write. The interpreter ([`apply_rules`]) owns all reading,
/// coercion, and omission logic; a row is the smallest unit of behavior.
enum Rule {
    /// `props[src]` as str → `tags[dst]`. The source value wins when present;
    /// `default = None` omits the tag when the source is missing or non-string,
    /// while `Some(d)` emits `d` as the fallback in that case.
    Str {
        src: &'static str,
        dst: &'static str,
        default: Option<&'static str>,
    },
    /// `props[src]` as f64 → `tags[dst]`, formatted via f64's `Display`
    /// (matches the prior `h.to_string()` byte-for-byte).
    F64 {
        src: &'static str,
        dst: &'static str,
    },
    /// `props[src]` as u64 → `tags[dst]`.
    U64 {
        src: &'static str,
        dst: &'static str,
    },
    /// When `props[src]` is bool `true`, emit `tags[dst] = val`. A missing or
    /// false value emits nothing.
    Flag {
        src: &'static str,
        dst: &'static str,
        val: &'static str,
    },
    /// `props[a][b]` as str → `tags[dst]` (the `names.primary` shape).
    Nested2 {
        a: &'static str,
        b: &'static str,
        dst: &'static str,
    },
}

const BUILDING_RULES: &[Rule] = &[
    Rule::Str {
        src: "class",
        dst: "building",
        default: Some("yes"),
    },
    Rule::F64 {
        src: "height",
        dst: "building:height",
    },
    Rule::U64 {
        src: "num_floors",
        dst: "building:levels",
    },
];

const TRANSPORTATION_RULES: &[Rule] = &[
    Rule::Str {
        src: "class",
        dst: "highway",
        default: Some("unclassified"),
    },
    Rule::Nested2 {
        a: "names",
        b: "primary",
        dst: "name",
    },
    Rule::Str {
        src: "road_surface",
        dst: "surface",
        default: None,
    },
    Rule::Flag {
        src: "is_bridge",
        dst: "bridge",
        val: "yes",
    },
    Rule::Flag {
        src: "is_tunnel",
        dst: "tunnel",
        val: "yes",
    },
];

const PLACE_RULES: &[Rule] = &[Rule::Nested2 {
    a: "names",
    b: "primary",
    dst: "name",
}];

const ADDRESS_RULES: &[Rule] = &[
    Rule::Str {
        src: "number",
        dst: "addr:housenumber",
        default: None,
    },
    Rule::Str {
        src: "street",
        dst: "addr:street",
        default: None,
    },
];

/// Apply each rule in order, writing into `tags`. Behavior is byte-identical
/// to the prior inline mapping code: missing sources are omitted unless a
/// default is set, f64/u64 use their `Display` form, and `Flag` only fires on
/// an explicit `true`.
fn apply_rules(props: &Value, rules: &[Rule], tags: &mut HashMap<String, String>) {
    for rule in rules {
        match *rule {
            Rule::Str { src, dst, default } => {
                let value = match (props.get(src).and_then(|v| v.as_str()), default) {
                    (Some(s), _) => Some(s.to_string()),
                    (None, Some(d)) => Some(d.to_string()),
                    (None, None) => None,
                };
                if let Some(v) = value {
                    tags.insert(dst.into(), v);
                }
            }
            Rule::F64 { src, dst } => {
                if let Some(n) = props.get(src).and_then(|v| v.as_f64()) {
                    tags.insert(dst.into(), n.to_string());
                }
            }
            Rule::U64 { src, dst } => {
                if let Some(n) = props.get(src).and_then(|v| v.as_u64()) {
                    tags.insert(dst.into(), n.to_string());
                }
            }
            Rule::Flag { src, dst, val } => {
                if props.get(src).and_then(|v| v.as_bool()).unwrap_or(false) {
                    tags.insert(dst.into(), val.into());
                }
            }
            Rule::Nested2 { a, b, dst } => {
                if let Some(s) = props.get(a).and_then(|v| v.get(b)).and_then(|v| v.as_str()) {
                    tags.insert(dst.into(), s.to_string());
                }
            }
        }
    }
}

/// Base-theme tag mapping, extracted verbatim from the prior inline arm.
///
/// Base is the one theme that does not fit the [`Rule`] shape: its groups are
/// irregular (water bodies emit a primary `natural=water` plus a conditional
/// `water=<subtype>`; the "natural from class" branch keys on both `subtype`
/// and `class`; the final fallback keys on `class` only). Forcing these into a
/// table would obscure the irregularity, so the `matches!` chain is preserved
/// here verbatim. Extracting it drops `map_tags_for_theme`'s complexity below
/// 10 while leaving this function the single owner of Base semantics.
fn map_base_tags(subtype: &str, class: &str, tags: &mut HashMap<String, String>) {
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

/// Map Overture feature properties to OSM-style tags for the given theme.
///
/// Thin dispatcher: the regular themes (Building, Transportation, Place,
/// Address) drive a declarative [`Rule`] table via [`apply_rules`]; Base's
/// irregular `subtype`/`class` classifier is delegated to [`map_base_tags`];
/// Place additionally runs its bespoke `categories.primary` →
/// [`map_place_category_to_osm_key`] block, which is a one-off key derivation
/// not worth a rule variant.
pub(super) fn map_tags_for_theme(props: &Value, theme: OvertureTheme) -> HashMap<String, String> {
    let mut tags: HashMap<String, String> = HashMap::new();

    match theme {
        OvertureTheme::Building => apply_rules(props, BUILDING_RULES, &mut tags),
        OvertureTheme::Transportation => apply_rules(props, TRANSPORTATION_RULES, &mut tags),
        OvertureTheme::Place => {
            apply_rules(props, PLACE_RULES, &mut tags);
            // categories.primary → amenity / shop / tourism / leisure
            if let Some(category) = props
                .get("categories")
                .and_then(|c| c.get("primary"))
                .and_then(|v| v.as_str())
            {
                let osm_key = map_place_category_to_osm_key(category);
                tags.insert(osm_key.into(), category.to_string());
            }
        }
        OvertureTheme::Base => {
            let subtype = props.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
            let class = props.get("class").and_then(|v| v.as_str()).unwrap_or("");
            map_base_tags(subtype, class, &mut tags);
        }
        OvertureTheme::Address => apply_rules(props, ADDRESS_RULES, &mut tags),
    }

    tags
}
