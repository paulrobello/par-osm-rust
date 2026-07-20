# ENH-002 — PBF Test Fixture + Format Coverage

> Status: proposed · Effort: Low-Medium (~half day) · Impact: closes the crate's largest test blind spot
> Best sequenced after AUDIT QA-101/QA-102 (parser policies settled), but independent of them.

## Goal

Give `parse_pbf` — currently an entirely untested public input format — a small
checked-in `.pbf` fixture plus integration tests that pin its behavior against the
XML parser's output for equivalent data, so future changes to the PBF path (QA-103's
clone removal, ARC-105's key constant, ARC-113's relation IDs) ship with coverage.

## Current State (verified at commit `55187f4`)

- `parse_pbf` (`src/osm/pbf.rs:109-226`) reads via `osmpbf::ElementReader::from_path`
  (BufReader-backed; see SEC-103's doc correction) and classifies nodes into
  poi/addr/tree/plain exactly like the XML parsers.
- No `.pbf` file exists anywhere in the repo (`find . -name '*.pbf'` is empty);
  only `parse_osm_file`'s extension dispatch and error branches are tested.
- `tests/integration.rs` (7 tests) holds cross-format/round-trip tests — the natural
  home for the new tests. XML fixtures are built inline as strings today.
- The `osmpbf` crate is read-only (no writer), so the fixture must be produced by an
  external tool once and committed as a binary.

## Implementation Steps

1. **Create the XML source fixture** `tests/fixtures/pbf_parity.osm` (new directory) —
   hand-written OSM XML, small and deliberately covering every classification branch:
   - 2 plain nodes (no tags) used by a way
   - 1 POI node (`amenity=cafe`, with `name`)
   - 1 address node (`addr:housenumber` + `addr:street`)
   - 1 tree node (`natural=tree`)
   - 1 closed way (`building=yes`, 4 node refs) and 1 open way (`highway=residential`)
   - 1 relation with a way member and a role
   - a `<bounds>` element
   Reuse tag names/shapes from the existing inline XML fixtures in
   `src/osm/xml_parse.rs` tests so classification expectations are identical.
2. **Generate the PBF fixture from it** using osmium-tool (installed locally, one time):

   ```bash
   brew install osmium-tool   # macOS; apt: osmium-tool
   osmium cat tests/fixtures/pbf_parity.osm -o tests/fixtures/pbf_parity.osm.pbf
   ```

   Commit BOTH files. Add `tests/fixtures/README.md` recording the exact generation
   command and the osmium version used, so the binary is reproducible.
   ⚠️ osmium requires `version`/`timestamp` attributes on elements in some modes — if
   `osmium cat` complains, regenerate the `.osm` with `version="1"` attributes on each
   element (the crate's parsers ignore them; verify by reading the attribute handling
   in `src/osm/xml_parse.rs` first).
3. **Parity test** in `tests/integration.rs`:

   ```rust
   #[test]
   fn parse_pbf_matches_parse_osm_xml_for_equivalent_fixture() {
       let xml = par_osm_rust::osm::parse_osm_file(Path::new("tests/fixtures/pbf_parity.osm")).unwrap();
       let pbf = par_osm_rust::osm::parse_osm_file(Path::new("tests/fixtures/pbf_parity.osm.pbf")).unwrap();
       assert_eq!(xml.nodes().len(), pbf.nodes().len());
       assert_eq!(xml.ways().len(), pbf.ways().len());
       assert_eq!(xml.poi_nodes.len(), pbf.poi_nodes.len());
       assert_eq!(xml.addr_nodes.len(), pbf.addr_nodes.len());
       assert_eq!(xml.tree_nodes.len(), pbf.tree_nodes.len());
       // Spot-check content, not just counts:
       // - the cafe POI's name matches in both
       // - the building way's node_refs match in both
       // - relation member roles match (NOTE: relation ids are not preserved pre-ARC-113)
   }
   ```

   Use the real public accessor names — check `src/osm/model.rs` for which fields are
   accessor-gated (`nodes()`, `ways()`, `ways_by_id()`) vs still `pub` (`poi_nodes`,
   `addr_nodes`, `tree_nodes`, `relations`, `bounds`) at the time of writing the test.
   ⚠️ `bounds`: osmium may or may not carry the `<bounds>` into the PBF header bbox,
   and `parse_pbf` may source bounds differently — read `parse_pbf`'s bounds handling
   first; if PBF bounds legitimately differ, assert them separately rather than in the
   parity block.
4. **Classification test** directly on the PBF: assert the specific POI is present in
   `poi_nodes` with `amenity == "cafe"` — this pins the PBF-side key list that
   ARC-105 will later consolidate.
5. **Error-path test**: a truncated copy of the fixture (write the first 40 bytes to a
   temp file) must return `Err` from `parse_pbf`, not panic.
6. Run the full suite; the new tests are pure-subset (no `blocking` feature needed) —
   confirm they pass under `cargo test --no-default-features` too.

## Files to Touch

- `tests/fixtures/pbf_parity.osm` (new)
- `tests/fixtures/pbf_parity.osm.pbf` (new, binary, ~1 KB)
- `tests/fixtures/README.md` (new — generation provenance)
- `tests/integration.rs` (new tests)

## Verification

```bash
cargo test --test integration
cargo test --no-default-features --test integration
make checkall
```

## Rollback

Delete the two fixture files and the added tests — no library code changes in this
plan, so rollback is purely subtractive. Keep the fixtures if only the tests need
rework; the binary is tiny and harmless.
