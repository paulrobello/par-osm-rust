//! OSM XML parsing.
//!
//! Exposes the single-pass string parser [`parse_osm_xml_str`], the streaming
//! file parser [`parse_osm_xml_file`] (ARC-013), and the two thin file-level
//! wrappers [`parse_osm_xml`] and [`parse_osm_file`]. The QA-005 attribute
//! helpers (`parse_bounds_attrs`, `parse_node_attrs`, `parse_tag_attrs`,
//! `parse_nd_ref`, `parse_member_attrs`) and the SEC-004 [`MAX_XML_DEPTH`]
//! cap are private to this submodule.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use anyhow::{Context, Result};
use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::BytesStart;
use quick_xml::events::Event;
use quick_xml::events::attributes::Attribute;

use super::model::{
    FeatureSource, OsmData, OsmNode, OsmPoiNode, OsmRelation, OsmWay, POI_TAG_KEYS, RelationMember,
};
use super::pbf::parse_pbf;

/// Maximum element nesting depth accepted by [`parse_osm_xml_str`].
///
/// quick-xml 0.41 is XXE/billion-laughs-safe by default, but unbounded
/// element nesting is the one residual denial-of-service vector. OSM XML
/// is effectively flat (depth 2-3: `<osm>` -> `<node>`/`<way>`/`<relation>`
/// -> `<tag>`/`<nd>`/`<member>`), so 64 is far above any legitimate input
/// while still bounding stack growth from a malicious payload. SEC-004.
const MAX_XML_DEPTH: usize = 64;

fn xml_attr_value(attr: &Attribute<'_>) -> String {
    attr.normalized_value(XmlVersion::Implicit1_0)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| {
            std::str::from_utf8(attr.value.as_ref())
                .unwrap_or("")
                .to_string()
        })
}

fn xml_attr_parse<T: std::str::FromStr>(attr: &Attribute<'_>) -> Option<T> {
    xml_attr_value(attr).parse().ok()
}

/// Read the `minlat`/`minlon`/`maxlat`/`maxlon` attributes from a
/// `<bounds>` element. Returns `None` if any of the four required
/// attributes is missing or unparseable.
fn parse_bounds_attrs(e: &BytesStart<'_>) -> Option<(f64, f64, f64, f64)> {
    let mut minlat: Option<f64> = None;
    let mut minlon: Option<f64> = None;
    let mut maxlat: Option<f64> = None;
    let mut maxlon: Option<f64> = None;
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"minlat" => minlat = xml_attr_parse(&attr),
            b"minlon" => minlon = xml_attr_parse(&attr),
            b"maxlat" => maxlat = xml_attr_parse(&attr),
            b"maxlon" => maxlon = xml_attr_parse(&attr),
            _ => {}
        }
    }
    Some((minlat?, minlon?, maxlat?, maxlon?))
}

/// Read the `id`/`lat`/`lon` attributes from a `<node>` element. Returns
/// `None` if any of the three required attributes is missing or unparseable.
fn parse_node_attrs(e: &BytesStart<'_>) -> Option<(i64, f64, f64)> {
    let mut id: Option<i64> = None;
    let mut lat: Option<f64> = None;
    let mut lon: Option<f64> = None;
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"id" => id = xml_attr_parse(&attr),
            b"lat" => lat = xml_attr_parse(&attr),
            b"lon" => lon = xml_attr_parse(&attr),
            _ => {}
        }
    }
    Some((id?, lat?, lon?))
}

/// Read the `k`/`v` attributes from a `<tag>` element. Returns `None` when
/// `k` is missing or empty, matching the parser's long-standing skip
/// behavior for tags without a key.
fn parse_tag_attrs(e: &BytesStart<'_>) -> Option<(String, String)> {
    let mut k = String::new();
    let mut v = String::new();
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"k" => k = xml_attr_value(&attr),
            b"v" => v = xml_attr_value(&attr),
            _ => {}
        }
    }
    if k.is_empty() { None } else { Some((k, v)) }
}

/// Read the `ref` attribute from an `<nd>` element. Returns `None` if
/// `ref` is missing or unparseable as an `i64`.
fn parse_nd_ref(e: &BytesStart<'_>) -> Option<i64> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == b"ref")
        .and_then(|a| xml_attr_parse::<i64>(&a))
}

/// Read the `type`/`ref`/`role` attributes from a `<member>` element.
/// Returns `(type, ref, role)` with `ref = 0` when missing/unparseable and
/// empty strings for `type`/`role` when absent. Callers decide which
/// member types to keep; the parser only retains `type="way"` members
/// with a non-zero ref.
fn parse_member_attrs(e: &BytesStart<'_>) -> (String, i64, String) {
    let mut mtype = String::new();
    let mut mref: i64 = 0;
    let mut mrole = String::new();
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"type" => mtype = xml_attr_value(&attr),
            b"ref" => mref = xml_attr_parse(&attr).unwrap_or(0),
            b"role" => mrole = xml_attr_value(&attr),
            _ => {}
        }
    }
    (mtype, mref, mrole)
}

/// Parse an OSM XML string into `OsmData`.
///
/// Single-pass: one `read_event_into` loop collects nodes, ways, relations,
/// and `<bounds>` in the order they appear, regardless of element ordering.
/// Overpass does not guarantee node-before-way ordering, but no position
/// resolution happens at parse time — ways store raw node-id references
/// (`OsmWay::node_refs`) and relations store raw way-id references
/// (`RelationMember::way_id`), so the parser does not need a complete
/// node-position map to emit ways or relations. Position resolution (e.g.
/// for clipping or rendering) is deferred to consumers.
///
/// Element nesting depth is capped at `MAX_XML_DEPTH` (SEC-004).
///
/// Both [`parse_osm_xml_str`] and [`parse_osm_xml_file`] route through the
/// same private `parse_osm_events` engine (QA-102): the string case
/// constructs `Reader::from_reader(xml.as_bytes())` (a `&[u8]` is
/// `BufRead`), so the streaming `read_event_into` shape runs against the
/// slice with no copy of the input. The two entry points are now
/// behaviorally identical by construction — only the reader source differs.
///
/// # Errors
///
/// Returns an error if the XML is malformed (including the `MAX_XML_DEPTH`
/// violation, SEC-004) or if a `<node>`/`<way>`/`<relation>` attribute
/// cannot be parsed. No I/O is performed so no I/O errors surface here;
/// see [`parse_osm_xml_file`] for the file-path equivalent.
///
/// # Examples
///
/// ```
/// use par_osm_rust::osm::parse_osm_xml_str;
///
/// let xml = r#"<?xml version="1.0"?>
/// <osm version="0.6">
///   <node id="1" lat="51.5" lon="-0.10"/>
///   <node id="2" lat="51.5" lon="-0.09"/>
/// </osm>"#;
///
/// let data = parse_osm_xml_str(xml)?;
/// assert_eq!(data.iter_ways().count(), 0);
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn parse_osm_xml_str(xml: &str) -> Result<OsmData> {
    let mut reader = Reader::from_reader(xml.as_bytes());
    reader.config_mut().trim_text(true);
    parse_osm_events(reader)
}

/// Stream a `.osm` XML file from disk into [`OsmData`] without first
/// reading the whole file into memory (ARC-013).
///
/// **Stream design.** The file is opened, wrapped in a `std::io::BufReader<File>`,
/// and handed to `quick_xml::Reader::from_reader`, which pulls bytes
/// incrementally. The event loop uses `quick_xml::Reader::read_event_into`
/// with a reused scratch `Vec<u8>` — the buffer-reusing API that a
/// streaming reader requires.
///
/// **Parser semantics are identical to [`parse_osm_xml_str`].** Both entry
/// points delegate to the same private `parse_osm_events` engine
/// (QA-102), so any change to the loop applies to both paths uniformly.
/// The same single-pass structure runs against the streamed events: nodes,
/// ways, relations, and `<bounds>` are collected in arrival order
/// regardless of element ordering (Overpass does not guarantee
/// node-before-way); the same attribute helpers (`parse_bounds_attrs`,
/// `parse_node_attrs`, `parse_tag_attrs`, `parse_nd_ref`,
/// `parse_member_attrs`) decode each element; the `MAX_XML_DEPTH` cap
/// (SEC-004) bounds element nesting; and the resulting [`OsmWay`]s carry
/// their OSM id (QA-021), fed into [`OsmData::new`]. For any valid file
/// the output equals
/// `parse_osm_xml_str(&std::fs::read_to_string(path)?)`.
///
/// **Memory profile.** Peak memory is bounded by:
/// * the `BufReader` internal buffer (8 KiB by default),
/// * the per-event scratch `Vec<u8>` (grows to the largest single XML
///   event — typically a few hundred bytes for OSM elements),
/// * the accumulated [`OsmData`] itself (inherent to the dataset).
///
/// The peak is **not** proportional to the input file size:
/// `std::fs::read_to_string`'s full-file `String` (roughly file-size bytes
/// on top of the parsed [`OsmData`]) is avoided. For a 200 MB `.osm`
/// extract that is the difference between ~400 MB peak (in-memory string
/// plus parsed data) and roughly the parsed data alone.
///
/// # Errors
///
/// Returns an error if the file cannot be opened or read, or if the XML
/// is malformed (including the `MAX_XML_DEPTH` violation, SEC-004).
///
/// # Examples
///
/// ```no_run
/// use par_osm_rust::osm::parse_osm_xml_file;
///
/// # fn main() -> anyhow::Result<()> {
/// let data = parse_osm_xml_file("/path/to/extract.osm")?;
/// println!("{} ways", data.iter_ways().count());
/// # Ok(())
/// # }
/// ```
pub fn parse_osm_xml_file<P: AsRef<Path>>(path: P) -> Result<OsmData> {
    let path_ref = path.as_ref();
    let file = File::open(path_ref).with_context(|| format!("opening {}", path_ref.display()))?;
    let bufreader = BufReader::new(file);
    let mut reader = Reader::from_reader(bufreader);
    reader.config_mut().trim_text(true);
    parse_osm_events(reader)
}

/// The unified single-pass XML event loop shared by [`parse_osm_xml_str`]
/// and [`parse_osm_xml_file`] (QA-102 / ARC-104).
///
/// Both entry points feed in a `quick_xml::Reader<R>` whose source is
/// either a `&[u8]` (string case) or a `BufReader<File>` (file case); both
/// are `BufRead`, so the streaming `read_event_into(&mut buf)` +
/// `buf.clear()` shape works uniformly. The depth cap, state machine, POI
/// / address / tree classification, and bounds accumulation logic live
/// here exactly once — they used to be duplicated across the two entry
/// points' ~200-line loop bodies.
fn parse_osm_events<R: std::io::BufRead>(mut reader: Reader<R>) -> Result<OsmData> {
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
    let mut explicit_bounds: Option<(f64, f64, f64, f64)> = None;

    // Per-element state. The three `in_*` flags are mutually exclusive
    // because OSM XML never nests `<node>`/`<way>`/`<relation>` inside
    // each other; their `<tag>`/`<nd>`/`<member>` children appear between
    // the Start and End events of the owning element.
    let mut in_node = false;
    let mut cur_lat = 0.0f64;
    let mut cur_lon = 0.0f64;
    let mut cur_node_tags: HashMap<String, String> = HashMap::new();

    let mut in_way = false;
    // QA-101: `Option<i64>` so a missing/unparseable way id is detected at the
    // way End event and the way skipped, rather than defaulted to 0 (which
    // collided two id-less ways and tripped the `OsmData::new` invariant).
    let mut current_way_id: Option<i64> = None;
    let mut current_tags: HashMap<String, String> = HashMap::new();
    let mut current_node_refs: Vec<i64> = Vec::new();
    // QA-101: first-wins duplicate guard, lives across the whole parse so two
    // `<way id="N">` elements in the same document do not both end up in the
    // output (which would also trip the `ways`/`ways_by_id` invariant).
    let mut seen_way_ids: HashSet<i64> = HashSet::new();

    let mut in_relation = false;
    let mut current_members: Vec<RelationMember> = Vec::new();

    let mut depth: usize = 0;

    // Reused scratch buffer for read_event_into — bounded by the largest
    // single XML event in the input, NOT by the source size.
    let mut buf: Vec<u8> = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                depth += 1;
                if depth > MAX_XML_DEPTH {
                    return Err(anyhow::anyhow!(
                        "XML element nesting depth {depth} exceeds limit {MAX_XML_DEPTH} at position {}",
                        reader.buffer_position()
                    ));
                }
                match e.name().as_ref() {
                    b"bounds" => {
                        if let Some(b) = parse_bounds_attrs(e) {
                            explicit_bounds = Some(b);
                        }
                    }
                    b"node" => {
                        if let Some((id, lat, lon)) = parse_node_attrs(e) {
                            min_lat = min_lat.min(lat);
                            min_lon = min_lon.min(lon);
                            max_lat = max_lat.max(lat);
                            max_lon = max_lon.max(lon);
                            // QA-101: node duplicate handling is intentionally
                            // last-wins (HashMap::insert overwrites). OSM
                            // extracts legitimately repeat node ids across
                            // ways; a duplicate node id with different coords
                            // is upstream data damage, out of scope here.
                            nodes.insert(id, OsmNode { lat, lon });
                            in_node = true;
                            cur_lat = lat;
                            cur_lon = lon;
                            cur_node_tags.clear();
                        }
                    }
                    b"way" => {
                        in_way = true;
                        // QA-101: keep the Option; the way End event skips the
                        // way if id is missing/unparseable, instead of defaulting
                        // to 0 and colliding with other id-less ways.
                        current_way_id = e
                            .attributes()
                            .flatten()
                            .find(|a| a.key.as_ref() == b"id")
                            .and_then(|a| xml_attr_parse::<i64>(&a));
                        current_tags.clear();
                        current_node_refs.clear();
                    }
                    b"relation" => {
                        in_relation = true;
                        current_tags.clear();
                        current_members.clear();
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => match e.name().as_ref() {
                b"bounds" => {
                    if let Some(b) = parse_bounds_attrs(e) {
                        explicit_bounds = Some(b);
                    }
                }
                b"node" => {
                    if let Some((id, lat, lon)) = parse_node_attrs(e) {
                        min_lat = min_lat.min(lat);
                        min_lon = min_lon.min(lon);
                        max_lat = max_lat.max(lat);
                        max_lon = max_lon.max(lon);
                        // QA-101: see the Start(b"node") arm — node-id duplicate
                        // handling is last-wins by design.
                        nodes.insert(id, OsmNode { lat, lon });
                    }
                }
                b"tag" if in_node || in_way || in_relation => {
                    if let Some((k, v)) = parse_tag_attrs(e) {
                        if in_node {
                            cur_node_tags.insert(k, v);
                        } else {
                            current_tags.insert(k, v);
                        }
                    }
                }
                b"nd" if in_way => {
                    if let Some(r) = parse_nd_ref(e) {
                        current_node_refs.push(r);
                    }
                }
                b"member" if in_relation => {
                    let (mtype, mref, mrole) = parse_member_attrs(e);
                    if mtype == "way" && mref != 0 {
                        current_members.push(RelationMember {
                            way_id: mref,
                            role: mrole,
                        });
                    }
                }
                _ => {}
            },
            Ok(Event::End(ref e)) => {
                depth = depth.saturating_sub(1);
                match e.name().as_ref() {
                    b"node" if in_node => {
                        in_node = false;
                        // QA-104: classify first, then construct each surviving
                        // entry once. A node can be both POI and addr; in that
                        // case one clone is unavoidable (two OsmPoiNode entries
                        // need the tag map) — clone for the first consumer and
                        // `mem::take` for the last. If exactly one classification
                        // fires, the tags move with zero clones.
                        let is_poi = cur_node_tags
                            .keys()
                            .any(|k| POI_TAG_KEYS.contains(&k.as_str()));
                        let is_addr = cur_node_tags.contains_key("addr:housenumber");
                        let is_tree =
                            cur_node_tags.get("natural").map(|s| s.as_str()) == Some("tree");
                        if is_poi && is_addr {
                            let tags = std::mem::take(&mut cur_node_tags);
                            poi_nodes.push(OsmPoiNode {
                                lat: cur_lat,
                                lon: cur_lon,
                                tags: tags.clone(),
                                source: FeatureSource::Osm,
                            });
                            addr_nodes.push(OsmPoiNode {
                                lat: cur_lat,
                                lon: cur_lon,
                                tags,
                                source: FeatureSource::Osm,
                            });
                        } else if is_poi {
                            poi_nodes.push(OsmPoiNode {
                                lat: cur_lat,
                                lon: cur_lon,
                                tags: std::mem::take(&mut cur_node_tags),
                                source: FeatureSource::Osm,
                            });
                        } else if is_addr {
                            addr_nodes.push(OsmPoiNode {
                                lat: cur_lat,
                                lon: cur_lon,
                                tags: std::mem::take(&mut cur_node_tags),
                                source: FeatureSource::Osm,
                            });
                        }
                        if is_tree {
                            tree_nodes.push(OsmNode {
                                lat: cur_lat,
                                lon: cur_lon,
                            });
                        }
                    }
                    b"way" if in_way => {
                        in_way = false;
                        // QA-101: skip ways with missing/unparseable id, and
                        // skip the second occurrence of any duplicate id
                        // (first-wins, matching `OsmData::merge`'s policy).
                        // Structured as a 3-way match so the post-event
                        // `buf.clear()` always runs (no `continue`).
                        match current_way_id {
                            Some(id) if seen_way_ids.insert(id) => {
                                // QA-104: `mem::take` avoids one HashMap + one
                                // Vec deep-clone per way; the next way's Start
                                // event clears the same buffers defensively
                                // (cheap no-op after take).
                                let way = OsmWay {
                                    id,
                                    tags: std::mem::take(&mut current_tags),
                                    node_refs: std::mem::take(&mut current_node_refs),
                                };
                                ways.push(way);
                            }
                            Some(id) => log::warn!("skipping duplicate way id {id}"),
                            None => log::warn!("skipping way with missing/invalid id"),
                        }
                    }
                    b"relation" if in_relation => {
                        in_relation = false;
                        let rel_type = current_tags.get("type").map(|s| s.as_str());
                        if rel_type == Some("multipolygon") && !current_members.is_empty() {
                            // QA-104: `mem::take` — relations fire on every
                            // multipolygon end, so this removes one HashMap +
                            // one Vec clone per relation.
                            relations.push(OsmRelation {
                                tags: std::mem::take(&mut current_tags),
                                members: std::mem::take(&mut current_members),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "XML parse error at position {}: {e}",
                    reader.buffer_position()
                ));
            }
            _ => {}
        }
        buf.clear();
    }

    let bounds = explicit_bounds
        .or_else(|| (min_lat < f64::MAX).then_some((min_lat, min_lon, max_lat, max_lon)));

    log::info!(
        "Parsed {} nodes, {} ways, {} relations, {} POI nodes, {} address nodes, {} tree nodes (XML)",
        nodes.len(),
        ways.len(),
        relations.len(),
        poi_nodes.len(),
        addr_nodes.len(),
        tree_nodes.len(),
    );

    Ok(OsmData::new(
        nodes, ways, relations, bounds, poi_nodes, addr_nodes, tree_nodes,
    ))
}

/// Parse a `.osm` XML file into `OsmData`.
///
/// Reads the entire file into a `String` and delegates to
/// [`parse_osm_xml_str`]. Prefer [`parse_osm_xml_file`] for large extracts:
/// it streams through a `BufReader` and avoids the full-file string.
///
/// # Errors
///
/// Returns `Err` if the file cannot be read, or if the parsed XML fails
/// [`parse_osm_xml_str`].
pub fn parse_osm_xml(path: &Path) -> Result<OsmData> {
    let xml =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_osm_xml_str(&xml)
}

/// Detect file format by extension and dispatch to the correct parser.
/// Supports `.osm.pbf` / `.pbf` (PBF format) and `.osm` (XML format).
///
/// # Errors
///
/// Returns `Err` if the extension is anything other than `.pbf` or `.osm`,
/// or if the dispatched-to parser ([`parse_pbf`] or [`parse_osm_xml`])
/// returns `Err`.
pub fn parse_osm_file(path: &Path) -> Result<OsmData> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "pbf" => parse_pbf(path),
        "osm" => parse_osm_xml(path),
        other => Err(anyhow::anyhow!(
            "unsupported file format '.{other}'; expected .osm.pbf or .osm"
        )),
    }
}
