use std::collections::HashMap;

use crate::filter::FeatureFilter;
use crate::osm::{FeatureSource, OsmData, OsmPoiNode};
use crate::overture::OvertureParams;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoiSourceMode {
    OsmOnly,
    OvertureOnly,
    Both,
    OverturePreferred,
}

impl Default for PoiSourceMode {
    fn default() -> Self {
        Self::OverturePreferred
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OvertureFailureMode {
    FallbackToOsm,
    Fail,
}

impl Default for OvertureFailureMode {
    fn default() -> Self {
        Self::FallbackToOsm
    }
}

#[derive(Debug, Clone)]
pub struct SourceOptions {
    pub filter: FeatureFilter,
    pub overpass_url: Option<String>,
    pub use_overpass_cache: bool,
    pub overture: OvertureParams,
    pub poi_source_mode: PoiSourceMode,
    pub overture_failure_mode: OvertureFailureMode,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceStatus {
    OsmOnly,
    OvertureOnly,
    Both,
    OverturePreferred,
    OvertureFallbackToOsm,
}

pub struct SourceFetchResult {
    pub data: OsmData,
    pub status: SourceStatus,
    pub warnings: Vec<String>,
}

fn normalized_name(tags: &HashMap<String, String>) -> Option<String> {
    tags.get("name")
        .map(|name| name.trim().to_lowercase())
        .filter(|name| !name.is_empty())
}

fn poi_category(tags: &HashMap<String, String>) -> String {
    for key in [
        "amenity", "shop", "tourism", "leisure", "historic", "man_made",
    ] {
        if let Some(value) = tags.get(key) {
            return format!("{key}:{value}");
        }
    }
    "unknown".to_string()
}

fn metres_between(a: &OsmPoiNode, b: &OsmPoiNode) -> f64 {
    let mean_lat = ((a.lat + b.lat) * 0.5).to_radians();
    let metres_per_degree_lat = 111_320.0;
    let metres_per_degree_lon = 111_320.0 * mean_lat.cos().abs().max(0.01);
    let dx = (a.lon - b.lon) * metres_per_degree_lon;
    let dz = (a.lat - b.lat) * metres_per_degree_lat;
    (dx * dx + dz * dz).sqrt()
}

fn poi_duplicates(a: &OsmPoiNode, b: &OsmPoiNode) -> bool {
    let same_category = poi_category(&a.tags) == poi_category(&b.tags);
    if !same_category {
        return false;
    }
    match (normalized_name(&a.tags), normalized_name(&b.tags)) {
        (Some(a_name), Some(b_name)) if a_name == b_name => metres_between(a, b) <= 25.0,
        (None, None) => metres_between(a, b) <= 10.0,
        _ => false,
    }
}

fn dedupe_pois_with_overture_preference(mut pois: Vec<OsmPoiNode>) -> Vec<OsmPoiNode> {
    pois.sort_by_key(|poi| match poi.source {
        FeatureSource::Overture => 0,
        FeatureSource::Osm => 1,
        FeatureSource::Synthetic => 2,
    });

    let mut kept: Vec<OsmPoiNode> = Vec::new();
    'next_poi: for poi in pois {
        for existing in &kept {
            if poi_duplicates(existing, &poi) {
                continue 'next_poi;
            }
        }
        kept.push(poi);
    }
    kept
}

pub fn merge_source_data(
    mut osm_data: OsmData,
    overture_data: Option<OsmData>,
    poi_source_mode: PoiSourceMode,
) -> SourceFetchResult {
    let original_osm_pois = osm_data.poi_nodes.clone();
    let mut warnings = Vec::new();

    match (poi_source_mode, overture_data) {
        (PoiSourceMode::OsmOnly, Some(mut overture)) => {
            overture.poi_nodes.clear();
            osm_data.merge(overture);
            osm_data.poi_nodes = original_osm_pois;
            SourceFetchResult {
                data: osm_data,
                status: SourceStatus::OsmOnly,
                warnings,
            }
        }
        (PoiSourceMode::OsmOnly, None) => SourceFetchResult {
            data: osm_data,
            status: SourceStatus::OsmOnly,
            warnings,
        },
        (PoiSourceMode::OvertureOnly, Some(mut overture)) => {
            let overture_pois = overture.poi_nodes.clone();
            osm_data.poi_nodes = overture_pois;
            overture.poi_nodes.clear();
            osm_data.merge(overture);
            SourceFetchResult {
                data: osm_data,
                status: SourceStatus::OvertureOnly,
                warnings,
            }
        }
        (PoiSourceMode::OvertureOnly, None) => {
            osm_data.poi_nodes.clear();
            warnings.push("Overture POIs unavailable for overture-only mode".to_string());
            SourceFetchResult {
                data: osm_data,
                status: SourceStatus::OvertureOnly,
                warnings,
            }
        }
        (PoiSourceMode::Both, Some(mut overture)) => {
            let mut all_pois = original_osm_pois;
            all_pois.extend(overture.poi_nodes.clone());
            overture.poi_nodes.clear();
            osm_data.merge(overture);
            osm_data.poi_nodes = dedupe_pois_with_overture_preference(all_pois);
            SourceFetchResult {
                data: osm_data,
                status: SourceStatus::Both,
                warnings,
            }
        }
        (PoiSourceMode::Both, None) => {
            warnings.push("Overture POIs unavailable; using OSM POIs only".to_string());
            SourceFetchResult {
                data: osm_data,
                status: SourceStatus::OvertureFallbackToOsm,
                warnings,
            }
        }
        (PoiSourceMode::OverturePreferred, Some(mut overture))
            if !overture.poi_nodes.is_empty() =>
        {
            let mut all_pois = original_osm_pois;
            all_pois.extend(overture.poi_nodes.clone());
            overture.poi_nodes.clear();
            osm_data.merge(overture);
            osm_data.poi_nodes = dedupe_pois_with_overture_preference(all_pois);
            SourceFetchResult {
                data: osm_data,
                status: SourceStatus::OverturePreferred,
                warnings,
            }
        }
        (PoiSourceMode::OverturePreferred, Some(mut overture)) => {
            warnings.push("Overture POIs unavailable; using OSM POIs only".to_string());
            overture.poi_nodes.clear();
            osm_data.merge(overture);
            osm_data.poi_nodes = original_osm_pois;
            SourceFetchResult {
                data: osm_data,
                status: SourceStatus::OvertureFallbackToOsm,
                warnings,
            }
        }
        (PoiSourceMode::OverturePreferred, None) => {
            warnings.push("Overture POIs unavailable; using OSM POIs only".to_string());
            SourceFetchResult {
                data: osm_data,
                status: SourceStatus::OvertureFallbackToOsm,
                warnings,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_data() -> OsmData {
        OsmData {
            nodes: HashMap::new(),
            ways: Vec::new(),
            ways_by_id: HashMap::new(),
            relations: Vec::new(),
            bounds: Some((0.0, 0.0, 1.0, 1.0)),
            poi_nodes: Vec::new(),
            addr_nodes: Vec::new(),
            tree_nodes: Vec::new(),
        }
    }

    fn poi(
        lat: f64,
        lon: f64,
        key: &str,
        value: &str,
        name: &str,
        source: FeatureSource,
    ) -> OsmPoiNode {
        let mut tags = HashMap::from([(key.to_string(), value.to_string())]);
        if !name.is_empty() {
            tags.insert("name".to_string(), name.to_string());
        }
        OsmPoiNode {
            lat,
            lon,
            tags,
            source,
        }
    }

    #[test]
    fn osm_only_keeps_osm_pois() {
        let mut osm = empty_data();
        osm.poi_nodes.push(poi(
            0.0,
            0.0,
            "amenity",
            "restaurant",
            "Diner",
            FeatureSource::Osm,
        ));
        let mut overture = empty_data();
        overture.poi_nodes.push(poi(
            0.0,
            0.0,
            "amenity",
            "restaurant",
            "Diner",
            FeatureSource::Overture,
        ));

        let merged = merge_source_data(osm, Some(overture), PoiSourceMode::OsmOnly);

        assert_eq!(merged.data.poi_nodes.len(), 1);
        assert_eq!(merged.data.poi_nodes[0].source, FeatureSource::Osm);
    }

    #[test]
    fn overture_only_keeps_overture_pois() {
        let mut osm = empty_data();
        osm.poi_nodes.push(poi(
            0.0,
            0.0,
            "amenity",
            "restaurant",
            "Diner",
            FeatureSource::Osm,
        ));
        let mut overture = empty_data();
        overture.poi_nodes.push(poi(
            0.0,
            0.0,
            "amenity",
            "restaurant",
            "Diner",
            FeatureSource::Overture,
        ));

        let merged = merge_source_data(osm, Some(overture), PoiSourceMode::OvertureOnly);

        assert_eq!(merged.data.poi_nodes.len(), 1);
        assert_eq!(merged.data.poi_nodes[0].source, FeatureSource::Overture);
    }

    #[test]
    fn overture_preferred_dedupes_named_pois_with_overture_winning() {
        let mut osm = empty_data();
        osm.poi_nodes.push(poi(
            51.50000,
            -0.10000,
            "amenity",
            "restaurant",
            "Diner",
            FeatureSource::Osm,
        ));
        let mut overture = empty_data();
        overture.poi_nodes.push(poi(
            51.50005,
            -0.10005,
            "amenity",
            "restaurant",
            "Diner",
            FeatureSource::Overture,
        ));

        let merged = merge_source_data(osm, Some(overture), PoiSourceMode::OverturePreferred);

        assert_eq!(merged.data.poi_nodes.len(), 1);
        assert_eq!(merged.data.poi_nodes[0].source, FeatureSource::Overture);
    }

    #[test]
    fn overture_preferred_falls_back_when_overture_missing() {
        let mut osm = empty_data();
        osm.poi_nodes.push(poi(
            0.0,
            0.0,
            "shop",
            "bakery",
            "Bakery",
            FeatureSource::Osm,
        ));

        let merged = merge_source_data(osm, None, PoiSourceMode::OverturePreferred);

        assert_eq!(merged.data.poi_nodes.len(), 1);
        assert_eq!(merged.data.poi_nodes[0].source, FeatureSource::Osm);
        assert!(
            merged
                .warnings
                .iter()
                .any(|warning| warning.contains("Overture POIs unavailable"))
        );
    }
}
