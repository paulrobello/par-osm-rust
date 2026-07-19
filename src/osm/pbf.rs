//! `.osm.pbf` reader.
//!
//! Exposes [`parse_pbf`], which reads a `.osm.pbf` file via `osmpbf`'s
//! `ElementReader` (backed by `BlobReader` over a `BufReader` — buffered I/O,
//! not memory-mapped) and folds its elements into an [`OsmData`].

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use osmpbf::{Element, ElementReader};

use super::model::{
    FeatureSource, OsmData, OsmNode, OsmPoiNode, OsmRelation, OsmWay, POI_TAG_KEYS, RelationMember,
};

/// Shared body of the `Element::Node` and `Element::DenseNode` branches in
/// [`parse_pbf`]. The two osmpbf element types expose the same `(id, lat, lon,
/// tags)` surface but through distinct types with no public shared trait, so
/// the branches resolve those four values and hand them off here (QA-004).
///
/// Updates the running bbox accumulator (`south`/`west`/`north`/
/// `east`), inserts the node into `nodes`, and classifies the tags to push
/// the node into the appropriate feature collection(s).
///
/// Each parameter maps 1:1 to a local accumulator already in scope at the
/// call site; the long signature is the cost of extracting the duplication
/// without restructuring the surrounding `parse_pbf` body.
#[allow(clippy::too_many_arguments)]
fn process_pbf_node(
    id: i64,
    lat: f64,
    lon: f64,
    tags: HashMap<String, String>,
    nodes: &mut HashMap<i64, OsmNode>,
    poi_nodes: &mut Vec<OsmPoiNode>,
    addr_nodes: &mut Vec<OsmPoiNode>,
    tree_nodes: &mut Vec<OsmNode>,
    min_lat: &mut f64,
    min_lon: &mut f64,
    max_lat: &mut f64,
    max_lon: &mut f64,
) {
    *min_lat = min_lat.min(lat);
    *min_lon = min_lon.min(lon);
    *max_lat = max_lat.max(lat);
    *max_lon = max_lon.max(lon);
    nodes.insert(id, OsmNode { lat, lon });
    if tags.keys().any(|k| POI_TAG_KEYS.contains(&k.as_str())) {
        poi_nodes.push(OsmPoiNode {
            lat,
            lon,
            tags: tags.clone(),
            source: FeatureSource::Osm,
        });
    }
    if tags.contains_key("addr:housenumber") {
        addr_nodes.push(OsmPoiNode {
            lat,
            lon,
            tags: tags.clone(),
            source: FeatureSource::Osm,
        });
    }
    if tags.get("natural").map(|s| s.as_str()) == Some("tree") {
        tree_nodes.push(OsmNode { lat, lon });
    }
}

/// Parse a `.osm.pbf` file into a full [`OsmData`].
///
/// Returns nodes, ways (each paired with its OSM id), multipolygon relations,
/// POI nodes entries (nodes carrying `amenity`/`shop`/`tourism`/`leisure`/
/// `historic`), address node entries (nodes carrying `addr:housenumber`),
/// individual tree positions (nodes carrying `natural=tree`), and the
/// dataset bounding box computed from the observed node lat/lon extrema.
///
/// # I/O model
///
/// `parse_pbf` reads `path` via `osmpbf`'s `ElementReader::from_path`, which
/// streams PBF blobs through a `BlobReader` over a `BufReader<File>` —
/// buffered I/O, not memory-mapped I/O. The advisory RUSTSEC-2026-0186 on
/// `memmap2` (a transitive dependency of `osmpbf`) is unreachable from this
/// crate: `osmpbf` only memory-maps when `mmap_blob` is used explicitly, and
/// the `ElementReader` path taken here never calls it. There is therefore no
/// `SIGBUS` hazard if the backing file is truncated, replaced, or concurrently
/// modified for the duration of the call (the reader will return an I/O
/// error instead).
///
/// # Errors
///
/// Returns an error if the file cannot be opened or read, or if the PBF
/// blob stream is malformed.
///
/// # Examples
///
/// ```no_run
/// use par_osm_rust::osm::parse_pbf;
/// use std::path::Path;
///
/// # fn main() -> anyhow::Result<()> {
/// let data = parse_pbf(Path::new("/path/to/planet.osm.pbf"))?;
/// println!("{} ways", data.iter_ways().count());
/// # Ok(())
/// # }
/// ```
pub fn parse_pbf(path: &Path) -> Result<OsmData> {
    let reader =
        ElementReader::from_path(path).with_context(|| format!("opening {}", path.display()))?;

    let mut nodes: HashMap<i64, OsmNode> = HashMap::new();
    let mut ways: Vec<OsmWay> = Vec::new();
    let mut relations: Vec<OsmRelation> = Vec::new();
    let mut poi_nodes: Vec<OsmPoiNode> = Vec::new();
    let mut addr_nodes: Vec<OsmPoiNode> = Vec::new();
    let mut tree_nodes: Vec<OsmNode> = Vec::new();
    let mut min_lat = f64::MAX;
    let mut min_lon = f64::MAX;
    let mut max_lat = f64::MIN;
    let mut max_lon = f64::MIN;
    // QA-101: first-wins duplicate guard, lives across the whole parse so two
    // ways with the same OSM id in the same PBF file do not both end up in the
    // output (which would trip the `OsmData::new` ways/ways_by_id invariant).
    let mut seen_way_ids: HashSet<i64> = HashSet::new();

    reader
        .for_each(|element| match element {
            Element::Node(n) => {
                let tags: HashMap<String, String> = n
                    .tags()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                process_pbf_node(
                    n.id(),
                    n.lat(),
                    n.lon(),
                    tags,
                    &mut nodes,
                    &mut poi_nodes,
                    &mut addr_nodes,
                    &mut tree_nodes,
                    &mut min_lat,
                    &mut min_lon,
                    &mut max_lat,
                    &mut max_lon,
                );
            }
            Element::DenseNode(n) => {
                let tags: HashMap<String, String> = n
                    .tags()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                process_pbf_node(
                    n.id(),
                    n.lat(),
                    n.lon(),
                    tags,
                    &mut nodes,
                    &mut poi_nodes,
                    &mut addr_nodes,
                    &mut tree_nodes,
                    &mut min_lat,
                    &mut min_lon,
                    &mut max_lat,
                    &mut max_lon,
                );
            }
            Element::Way(w) => {
                let id = w.id();
                // QA-101: identical policy to the XML parser. PBF's `Way::id`
                // returns the protobuf field directly (always an i64); the
                // protobuf default for an absent field is 0, which we treat
                // as "missing/invalid" — OSM's id allocator never issues 0,
                // and two id-less ways colliding on 0 is exactly the bug this
                // guard prevents. `seen_way_ids` then handles real duplicates
                // from concatenated extracts (first-wins, matching
                // `OsmData::merge`).
                if id == 0 {
                    log::warn!("skipping way with missing/invalid id");
                    return;
                }
                if !seen_way_ids.insert(id) {
                    log::warn!("skipping duplicate way id {id}");
                    return;
                }
                let tags: HashMap<String, String> = w
                    .tags()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                let node_refs: Vec<i64> = w.refs().collect();
                // QA-103: move both locals — they are not reused after
                // construction; the previous `tags.clone()` / `node_refs.clone()`
                // was pure waste on the PBF hot path.
                let way = OsmWay {
                    id,
                    tags,
                    node_refs,
                };
                ways.push(way);
            }
            Element::Relation(r) => {
                let rel_type = r.tags().find(|(k, _)| *k == "type").map(|(_, v)| v);
                if rel_type == Some("multipolygon") {
                    // ARC-113: skip relations with missing/invalid id (id == 0
                    // in PBF indicates an absent protobuf field, mirroring the
                    // QA-101 way-id policy). A relation without an id cannot
                    // be usefully referenced downstream.
                    let id = r.id();
                    if id == 0 {
                        log::warn!("skipping relation with missing/invalid id");
                        return;
                    }
                    let tags: HashMap<String, String> = r
                        .tags()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect();
                    let members: Vec<RelationMember> = r
                        .members()
                        .filter_map(|m| {
                            if matches!(m.member_type, osmpbf::elements::RelMemberType::Way) {
                                Some(RelationMember {
                                    way_id: m.member_id,
                                    role: m.role().unwrap_or_default().to_string(),
                                })
                            } else {
                                None
                            }
                        })
                        .collect();
                    if !members.is_empty() {
                        relations.push(OsmRelation { id, tags, members });
                    }
                }
            }
        })
        .context("reading PBF elements")?;

    let bounds = if min_lat < f64::MAX {
        Some((min_lat, min_lon, max_lat, max_lon))
    } else {
        None
    };

    log::info!(
        "Parsed {} nodes, {} ways, {} relations, {} POI nodes, {} address nodes, {} tree nodes",
        nodes.len(),
        ways.len(),
        relations.len(),
        poi_nodes.len(),
        addr_nodes.len(),
        tree_nodes.len()
    );

    Ok(OsmData::default()
        .with_nodes(nodes)
        .with_ways(ways)
        .with_relations(relations)
        .with_bounds(bounds)
        .with_poi_nodes(poi_nodes)
        .with_addr_nodes(addr_nodes)
        .with_tree_nodes(tree_nodes))
}
