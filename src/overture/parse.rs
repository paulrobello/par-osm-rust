//! Pure Overture GeoJSON → [`crate::osm::OsmData`] conversion.
//!
//! No I/O, no feature gates. Owned by [`super`] and re-exported at
//! `crate::overture::parse_overture_geojson` (ARC-007 / QA-009).

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;

use crate::osm::{FeatureSource, OsmData, OsmNode, OsmPoiNode, OsmWay};
use crate::synthetic_ids::OvertureIdAllocator;

use super::theme::{OvertureTheme, map_tags_for_theme};

// ── Synthetic node-ID allocation ──────────────────────────────────────────
//
// Overture geometry nodes and ways do not carry OSM IDs, so each parse
// assigns synthetic IDs from an [`OvertureIdAllocator`]. A single allocator
// is threaded through every parse call within one fetch so multi-theme
// merges never collide (ARC-101); the public `parse_overture_geojson`
// constructs a fresh allocator per call to preserve the ARC-009 / QA-010
// per-parse determinism contract for standalone callers. The allocator
// starts at `SYNTHETIC_OVERTURE_ID_BASE` and decrements per ID, keeping the
// Overture range disjoint from the writer's node/way/relation ranges and
// from real OSM IDs. See `crate::synthetic_ids` for the centralized
// contract.

/// Update a running bounding-box accumulator with a new coordinate.
fn update_bounds(
    min_lat: &mut f64,
    min_lon: &mut f64,
    max_lat: &mut f64,
    max_lon: &mut f64,
    lat: f64,
    lon: f64,
) {
    *min_lat = min_lat.min(lat);
    *min_lon = min_lon.min(lon);
    *max_lat = max_lat.max(lat);
    *max_lon = max_lon.max(lon);
}

/// Convert a GeoJSON coordinate array `[lon, lat]` or `[lon, lat, ele]` to an
/// `(OsmNode, i64)` pair and update the bounding-box accumulator.
///
/// Returns the synthetic node ID (drawn from `id_alloc`) and the node, or
/// `None` if the array is malformed.
fn coord_to_node(
    coord: &Value,
    id_alloc: &mut OvertureIdAllocator,
    min_lat: &mut f64,
    min_lon: &mut f64,
    max_lat: &mut f64,
    max_lon: &mut f64,
) -> Option<(i64, OsmNode)> {
    let arr = coord.as_array()?;
    let lon = arr.first()?.as_f64()?;
    let lat = arr.get(1)?.as_f64()?;
    update_bounds(min_lat, min_lon, max_lat, max_lon, lat, lon);
    Some((id_alloc.next_id(), OsmNode { lat, lon }))
}

/// Convert a GeoJSON coordinate array (ring or line) into a list of node IDs
/// and the corresponding node map entries.
///
/// Each element of `coords` is expected to be a `[lon, lat]` array. IDs are
/// drawn from `id_alloc`.
fn coords_to_nodes(
    coords: &[Value],
    id_alloc: &mut OvertureIdAllocator,
    min_lat: &mut f64,
    min_lon: &mut f64,
    max_lat: &mut f64,
    max_lon: &mut f64,
) -> (Vec<i64>, HashMap<i64, OsmNode>) {
    let mut node_refs = Vec::with_capacity(coords.len());
    let mut nodes = HashMap::with_capacity(coords.len());
    for coord in coords {
        if let Some((id, node)) = coord_to_node(coord, id_alloc, min_lat, min_lon, max_lat, max_lon)
        {
            node_refs.push(id);
            nodes.insert(id, node);
        }
    }
    (node_refs, nodes)
}

/// Build one way from a coordinate ring/line, appending it (and any new nodes)
/// to the running accumulators. Shared by the LineString / Polygon /
/// MultiPolygon branches of [`parse_overture_geojson`] (QA-006).
///
/// Behavior preserved exactly from the prior inlined branches:
/// - If `coords` produces zero valid node refs, nothing is pushed.
/// - Otherwise a synthetic way ID is allocated from `id_alloc` and stored
///   directly on the [`OsmWay`] (QA-021), the way is appended with `tags`
///   (moved), and the new nodes are merged into `nodes`.
///
/// The synthetic id is drawn from the per-parse [`OvertureIdAllocator`] so
/// identical GeoJSON inputs produce identical id sequences across calls
/// (ARC-009 / QA-010 determinism).
///
/// Argument count exceeds clippy's default threshold because the four
/// bounding-box accumulators are passed individually, mirroring the existing
/// `coord_to_node` / `coords_to_nodes` style. Bundling them into a struct
/// would require touching those helpers too, which is out of scope for this
/// dedupe pass (QA-006).
#[allow(clippy::too_many_arguments)]
fn push_way_from_coords(
    coords: &[Value],
    id_alloc: &mut OvertureIdAllocator,
    nodes: &mut HashMap<i64, OsmNode>,
    ways: &mut Vec<OsmWay>,
    tags: HashMap<String, String>,
    min_lat: &mut f64,
    min_lon: &mut f64,
    max_lat: &mut f64,
    max_lon: &mut f64,
) {
    let (node_refs, new_nodes) =
        coords_to_nodes(coords, id_alloc, min_lat, min_lon, max_lat, max_lon);
    if node_refs.is_empty() {
        return;
    }
    let way_id = id_alloc.next_id();
    ways.push(OsmWay {
        id: way_id,
        tags,
        node_refs,
    });
    nodes.extend(new_nodes);
}

/// Parse an Overture GeoJSON `FeatureCollection` string into an [`OsmData`].
///
/// Each GeoJSON feature is converted according to `theme`:
///
/// - `Point` geometries become POI nodes (Place theme) or address nodes (Address theme).
/// - `LineString` geometries become ways.
/// - `Polygon` geometries become ways using the outer ring.
/// - `MultiPolygon` geometries produce one way per polygon outer ring.
///
/// Synthetic negative node IDs are assigned to avoid collision with OSM IDs.
///
/// Constructs a fresh [`OvertureIdAllocator`] for this single call, so two
/// parses of identical GeoJSON produce identical ID sequences (ARC-009 /
/// QA-010). For multi-theme fetches that merge results into one
/// [`OsmData`], the fetch orchestrator must instead thread a single
/// allocator through [`parse_overture_geojson_with_allocator`] so the
/// merged ways/`ways_by_id` invariant cannot be violated by two themes
/// emitting the same IDs (ARC-101).
pub fn parse_overture_geojson(geojson_str: &str, theme: OvertureTheme) -> Result<OsmData> {
    let mut id_alloc = OvertureIdAllocator::new();
    parse_overture_geojson_with_allocator(geojson_str, theme, &mut id_alloc)
}

/// Crate-internal variant of [`parse_overture_geojson`] that draws synthetic
/// IDs from a caller-owned [`OvertureIdAllocator`] instead of constructing a
/// fresh one.
///
/// Multi-theme fetch orchestration (see
/// `crate::overture::cli::fetch_overture_data`) constructs **one** allocator
/// per fetch and threads it through every per-theme parse call. Because a
/// single allocator never reissues an ID, the merged ways across all themes
/// carry disjoint IDs and [`OsmData::merge`] preserves the `ways` /
/// `ways_by_id` invariant (ARC-101). The caller still gets ARC-009 / QA-010
/// determinism at the fetch granularity: identical fetch inputs (bbox +
/// theme set + allocator base) yield identical ID sequences.
pub(crate) fn parse_overture_geojson_with_allocator(
    geojson_str: &str,
    theme: OvertureTheme,
    id_alloc: &mut OvertureIdAllocator,
) -> Result<OsmData> {
    let root: Value = serde_json::from_str(geojson_str).context("parsing Overture GeoJSON")?;

    let features = root
        .get("features")
        .and_then(|f| f.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[]);

    let mut nodes: HashMap<i64, OsmNode> = HashMap::new();
    let mut ways: Vec<OsmWay> = Vec::new();
    let mut poi_nodes: Vec<OsmPoiNode> = Vec::new();
    let mut addr_nodes: Vec<OsmPoiNode> = Vec::new();
    let mut tree_nodes: Vec<OsmNode> = Vec::new();

    let mut min_lat = f64::MAX;
    let mut min_lon = f64::MAX;
    let mut max_lat = f64::MIN;
    let mut max_lon = f64::MIN;

    for feature in features {
        let props = feature.get("properties").unwrap_or(&Value::Null);
        let tags = map_tags_for_theme(props, theme);

        let geometry = match feature.get("geometry") {
            Some(g) => g,
            None => continue,
        };
        let geom_type = geometry.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let coordinates = geometry.get("coordinates");

        match geom_type {
            "Point" => {
                if let Some(coord) = coordinates
                    && let Some((id, node)) = coord_to_node(
                        coord,
                        id_alloc,
                        &mut min_lat,
                        &mut min_lon,
                        &mut max_lat,
                        &mut max_lon,
                    )
                {
                    nodes.insert(id, node);
                    let poi = OsmPoiNode {
                        lat: node.lat,
                        lon: node.lon,
                        tags: tags.clone(),
                        source: FeatureSource::Overture,
                    };
                    match theme {
                        OvertureTheme::Address => addr_nodes.push(poi),
                        OvertureTheme::Place => poi_nodes.push(poi),
                        _ => {
                            // Decorative tree nodes from land theme
                            if tags.get("natural").map(|s| s.as_str()) == Some("tree") {
                                tree_nodes.push(OsmNode {
                                    lat: node.lat,
                                    lon: node.lon,
                                });
                            }
                        }
                    }
                }
            }

            "LineString" => {
                if let Some(coords) = coordinates.and_then(|c| c.as_array()) {
                    push_way_from_coords(
                        coords,
                        id_alloc,
                        &mut nodes,
                        &mut ways,
                        tags,
                        &mut min_lat,
                        &mut min_lon,
                        &mut max_lat,
                        &mut max_lon,
                    );
                }
            }

            "Polygon" => {
                // Use the outer ring (first element).
                if let Some(outer_ring) = coordinates
                    .and_then(|c| c.as_array())
                    .and_then(|rings| rings.first())
                    .and_then(|r| r.as_array())
                {
                    push_way_from_coords(
                        outer_ring,
                        id_alloc,
                        &mut nodes,
                        &mut ways,
                        tags,
                        &mut min_lat,
                        &mut min_lon,
                        &mut max_lat,
                        &mut max_lon,
                    );
                }
            }

            "MultiPolygon" => {
                // Each polygon produces one way from its outer ring.
                if let Some(polygons) = coordinates.and_then(|c| c.as_array()) {
                    for polygon in polygons {
                        if let Some(outer_ring) = polygon
                            .as_array()
                            .and_then(|rings| rings.first())
                            .and_then(|r| r.as_array())
                        {
                            // `tags` is cloned per polygon so each ring gets
                            // its own copy; the original is dropped at the
                            // end of the arm.
                            push_way_from_coords(
                                outer_ring,
                                id_alloc,
                                &mut nodes,
                                &mut ways,
                                tags.clone(),
                                &mut min_lat,
                                &mut min_lon,
                                &mut max_lat,
                                &mut max_lon,
                            );
                        }
                    }
                }
            }

            _ => {
                // Unknown geometry type — skip.
            }
        }
    }

    let bounds = if min_lat < f64::MAX {
        Some((min_lat, min_lon, max_lat, max_lon))
    } else {
        None
    };

    Ok(OsmData::new(
        nodes,
        ways,
        Vec::new(),
        bounds,
        poi_nodes,
        addr_nodes,
        tree_nodes,
    ))
}
