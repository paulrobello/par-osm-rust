# Test fixtures

Binary and text fixtures used by `tests/integration.rs`. These live outside the
crate source so they are available to integration tests (which run from the
crate root) but do not pollute the library build.

## `pbf_parity.osm` / `pbf_parity.osm.pbf`

A small, hand-written OSM dataset exercising every classification branch the
crate's parsers must handle: plain nodes, a POI node, an address node, a tree
node, an open way, a closed way, and a multipolygon relation. The XML source
(`pbf_parity.osm`) and the PBF twin (`pbf_parity.osm.pbf`) describe identical
data, so `tests/integration.rs::parse_pbf_matches_parse_osm_xml_for_equivalent_fixture`
can assert the PBF parser produces the same `OsmData` as the XML parser.

`parse_pbf` is an entire public input format that, prior to ENH-002, had **zero**
test coverage — no `.pbf` fixture existed anywhere in the repo. These two files
close that blind spot and protect every future change to the PBF path
(classification, relation ids, the duplicate-id policy).

### Regenerating the PBF

The `osmpbf` crate is read-only, so the `.pbf` cannot be produced in-test. It is
generated once from the XML source with [osmium-tool](https://osmcode.org/osmium-tool/)
and committed (~600 bytes):

```bash
osmium cat tests/fixtures/pbf_parity.osm -o tests/fixtures/pbf_parity.osm.pbf
```

The committed `pbf_parity.osm.pbf` was produced with **osmium-tool 1.19.1**.

If the XML source is edited, regenerate the `.pbf` with the command above and
re-run `cargo test --test integration`. The XML parser and the PBF parser share
no classification code, so a fixture change that keeps the two files in sync
keeps the parity test green.
