# Contributing to par-osm-rust

This guide covers setup, the verification gate, commit and PR conventions, and
the patterns to follow when changing the OSM/Overture source pipeline. It is
aimed at contributors who want to land a change in `par-osm-rust`, the shared
data-source crate used by `osm-to-bedrock` and `osm-world`.

## Table of Contents

- [Setup](#setup)
- [The Verification Gate](#the-verification-gate)
- [Pre-commit Hooks](#pre-commit-hooks)
- [Conventional Commits](#conventional-commits)
- [Branches and Pull Requests](#branches-and-pull-requests)
- [Rustdoc Style](#rustdoc-style)
- [Testing Conventions](#testing-conventions)
- [Walkthrough: Adding a New OvertureTheme Variant](#walkthrough-adding-a-new-overturetheme-variant)
- [Cache-Migration Startup Contract](#cache-migration-startup-contract)
- [Related Documentation](#related-documentation)

## Setup

`par-osm-rust` targets **Rust edition 2024** with an **MSRV of 1.88**
(declared in `Cargo.toml`). Any stable toolchain at or above 1.88 builds the
crate; CI runs on `dtolnay/rust-toolchain@stable`.

```bash
git clone https://github.com/paulrobello/par-osm-rust.git
cd par-osm-rust
cargo build
```

There is no networked build step. The optional `overturemaps` CLI is a
**runtime** dependency only; it is not required to build, test, or publish the
crate, and tests that exercise Overture paths use fixture GeoJSON rather than
the CLI.

## The Verification Gate

Run `make checkall` before pushing. It runs the same four stages CI runs, in
the same order:

```bash
make checkall
```

The target expands to:

| Stage | Command | Purpose |
| --- | --- | --- |
| `fmt-check` | `cargo fmt -- --check` | Rejects unformatted code |
| `lint` | `cargo clippy --all-targets --all-features -- -D warnings` | Denies any clippy warning |
| `typecheck` | `cargo check --all-targets` | Catches type errors across targets |
| `test` | `cargo test --all-features` | Runs the full test suite with all features on |

`--all-features` matters today and tomorrow. The `blocking` feature
(gated by `default = ["blocking"]` in `Cargo.toml`) pulls in the
`reqwest`-based network surface — the `overpass` and `srtm` modules plus
the Overture CLI orchestration in `overture::cli`. `--all-features` and
the default build are currently the same set (the only feature is the
default-on `blocking`), so the gate is future-proofing: a PR that passes
`cargo test` but fails `cargo test --all-features` is not ready to merge,
and `cargo test --no-default-features` must also stay green so the pure
subset (data model, parsing, writing, cache I/O, filter, synthetic IDs,
elevation) keeps compiling without the network stack.

For narrower work, the individual targets (`make fmt`, `make lint`,
`make typecheck`, `make test`) are available. The full gate is the source of
truth — do not report a change as done until `make checkall` is clean.

## Pre-commit Hooks

A `.pre-commit-config.yaml` is committed at the repository root. Install the
hooks once after cloning:

```bash
pre-commit install
```

The configuration runs `gitleaks` and `detect-private-key` for secret
scanning, plus the Make-backed `fmt`, `lint`, and `typecheck` hooks so the
same checks that gate CI also gate your local commits. To run every hook
across the whole tree on demand:

```bash
make pre-commit
```

If a hook flags a false positive, fix the finding rather than bypassing the
hook. Credentials and tokens must never be committed; the
`.claude/settings.local.json`, `.env`, `*.pem`, `*.key`, and `secrets.*`
patterns in `.gitignore` are defense-in-depth, not a substitute for review.

## Conventional Commits

This repository uses [Conventional Commits](https://www.conventionalcommits.org/).
Scope each commit with one of the established types:

| Type | Use for |
| --- | --- |
| `feat` | New user-facing capability (a new theme, a new public API) |
| `fix` | Bug fix or correctness improvement |
| `perf` | Performance change that does not alter behavior |
| `refactor` | Code reorganization with no behavior change |
| `docs` | Documentation only — README, ARCHITECTURE, docstrings, this file |
| `test` | Test additions or fixes |
| `chore` | Tooling, CI, dependency bumps, formatting |
| `ci` | GitHub Actions workflow changes |

Examples from this repository's history:

```text
feat: donate source_options parsers from osm-to-bedrock (ARC-011)
fix(overpass): block redirects and cap error body (SEC-002, SEC-005)
docs(architecture): sync ARCHITECTURE.md to 0.2.0 implementation
chore: bump quick-xml 0.39→0.41 and migrate to normalized_value API
```

Reference the audit or issue ID in parentheses when one exists. Breaking
changes must be called out in the commit body with a `BREAKING CHANGE:` footer
so the next version bump is unambiguous.

## Branches and Pull Requests

- Branch from `main` and target `main`. Use a descriptive kebab-case branch
  name prefixed by the work type, for example `fix/overpass-redirect-policy`
  or `docs/contributing-and-changelog`.
- Keep PRs focused: one logical change per PR. A PR that lands a behavior
  change, a refactor, and a dependency bump is harder to review and revert.
- Open the PR with a summary of what changed and why, link any related audit
  IDs (`ARC-`, `SEC-`, `QA-`, `DOC-`), and note any downstream coordination
  needed for breaking changes (the consumers are `osm-to-bedrock` and
  `osm-world`).
- `make checkall` must be green before requesting review. The CI workflow
  re-runs it on every push; do not rely on CI to catch what you can catch
  locally.

## Rustdoc Style

The gold standard is `src/sources.rs` — every public item is documented and
every fallible function carries the standard sections. Follow that file when
adding or editing public APIs.

Conventions:

- One short summary sentence, blank line, then the longer explanation.
- Document every `pub` item, `pub` field, and `pub` enum variant. The crate
  enforces this via `#![warn(missing_docs)]` in `src/lib.rs`, and
  `cargo clippy --all-features -- -D warnings` turns the warning into a
  build failure — so the gate stays green by construction. New public code
  that skips doc comments fails CI immediately.
- Include `# Errors` on every public function returning `Result`, naming
  the conditions that produce `Err` (one to three lines, derived from the
  function's actual `bail!`/`?` paths — never guess). The convention is
  applied uniformly: SEC-101/102/104/105 documented the validators and
  I/O paths; DOC-002 closed the remaining gap across the parser, cache,
  and option-parser surfaces.
- Include `# Panics` only where a public function can genuinely panic. The
  current library has no such functions — debug asserts under
  `debug_assertions` (e.g. `OsmData::validate_invariants`) do not count
  per rustdoc convention. If you add an indexing, slice, or arithmetic
  operation that can panic on bad input, document the precondition.
- Cross-reference other items with intra-doc links (square brackets wrapping
  a backtick-quoted symbol, e.g. `SourceOptions` or `fetch_map_data`) so
  rustdoc renders them as links. Use plain backticks (no square brackets) when
  referring to private or `pub(crate)` items — intra-doc links to non-`pub`
  items emit a `private_intra_doc_links` warning, which the `-D warnings` gate
  fails.
- Mark example blocks with ` ```no_run ` when they touch the network or the
  filesystem, so `cargo test --doc` does not attempt to execute them.

The documentation CI job (`.github/workflows/ci.yml`, `docs` and `docs-lint`)
runs `RUSTDOCFLAGS=-D warnings cargo doc --no-deps --all-features`,
markdownlint-cli2 over `README.md`, `CONTRIBUTING.md`, `CHANGELOG.md`, and
`docs/`, and lychee link-checking over the same set. A broken intra-doc
link, an undocumented public item, or a stale markdown link each fail the
build.

## Testing Conventions

Tests live in three places: inline `#[cfg(test)] mod tests` blocks at the
bottom of each module (the bulk of the suite, holding their own fixtures
and assertions), cross-format and round-trip tests under `tests/` (today a
single `tests/integration.rs` covering XML round-trip, PBF→XML parity
assertions, and merge-policy scenarios that span modules), and criterion
benches under `benches/` (`parse_osm_xml`, `write_osm_xml`,
`merge_source_data`) that double as perf-regression guards with embedded
correctness assertions. Put new unit tests in the inline module that owns
the code under test; reach for `tests/integration.rs` when a test needs to
combine more than one public module.

For source orchestration, use the dependency-injection seam. The public
`fetch_map_data` delegates to `fetch_map_data_with_fetchers`, which takes the
OSM and Overture fetch functions as generic parameters:

```rust,ignore
pub(crate) fn fetch_map_data_with_fetchers<FetchOsm, FetchOverture>(
    bbox: (f64, f64, f64, f64),
    options: &SourceOptions,
    progress_cb: &mut dyn FnMut(f32, &str),
    fetch_osm: FetchOsm,
    fetch_overture: FetchOverture,
) -> Result<SourceFetchResult>
where
    FetchOsm: FnMut((f64, f64, f64, f64), &FeatureFilter, bool, &str) -> Result<OsmData>,
    FetchOverture: FnMut((f64, f64, f64, f64), &OvertureParams, &mut dyn FnMut(f32, &str)) -> Result<OsmData>,
```

This separates the pure merge policy (`merge_source_data`, no I/O) from the
side-effecting fetch, so tests can drive every `PoiSourceMode` and
`OvertureFailureMode` branch with synthetic fetch closures and deterministic
`OsmData` fixtures. Prefer this seam over mocking HTTP.

When you add or change behavior, add a test that fails without the change and
passes with it. Known coverage gaps the audit flagged are tracked inline at
the call sites that miss them (notably `.pbf` fixture coverage for
`parse_pbf`, and broader parser-equivalence fixtures); contributions that
close those gaps are welcome.

## Walkthrough: Adding a New OvertureTheme Variant

This walkthrough enumerates every spot that must change when adding a new
`OvertureTheme` variant. It is the canonical example of a cross-cutting change
in this crate, because themes touch the enum, its mappings, parsing, the
tag-mapping layer, the feature-routing layer, and tests. Since 0.2.0 the
single `src/overture.rs` file has been split into `src/overture/{theme,parse,cache,cli}.rs`
plus a thin `src/overture/mod.rs` re-exporting them (ARC-007 / QA-009), so
the edits are spread across that subtree.

Suppose you are adding a new theme, `OvertureTheme::Water` (used here purely
as an example; the real `Base` theme already covers water).

1. **The enum itself.** Add the variant to `pub enum OvertureTheme` in
   `src/overture/theme.rs` with a rustdoc comment describing what the theme
   represents.

2. **`OvertureTheme::all()`.** Add the variant to the canonical ordering.
   This is the default theme list returned to applications that do not
   specify themes explicitly.

3. **`OvertureTheme::cli_types()`.** Map the new variant to one or more
   `overturemaps download --type` values. For example, `Base` maps to
   `["land", "land_use", "water"]`. The strings must match the upstream CLI's
   accepted type names exactly.

4. **`OvertureTheme::from_str_loose()`.** Accept the user-facing aliases the
   theme should answer to — singular, plural, and any short forms. Look at how
   `Address` accepts `"address"`, `"addresses"`, and `"addr"`, and how
   `Base` accepts `"base"`, `"land"`, `"land_use"`, `"landuse"`, and
   `"water"`.

5. **`impl Display for OvertureTheme`** in `src/overture/theme.rs`. Add the
   canonical lowercase string for the new variant. This is used in logs,
   warnings, and the cache key.

6. **`map_tags_for_theme()`** in `src/overture/theme.rs`. Add a match arm
   that converts Overture feature properties into OSM tags for the new theme.
   This is where Overture's property schema becomes OSM-flavored tags that
   downstream consumers can render.

7. **`parse_overture_geojson()`** in `src/overture/parse.rs`. Add a match
   arm in the feature-routing branch that decides which `OsmData` collection
   a feature of the new theme lands in (`poi_nodes`, `addr_nodes`, or a way
   collection). For example, `Place` routes to `poi_nodes` and `Address`
   routes to `addr_nodes`.

8. **Cache-key impact** in `src/overture/cache.rs`. The version-aware
   `overture_cache_key_with_version` folds `cli_type` strings into the
   SHA-256 input, so a new theme's `cli_types()` automatically produces
   distinct keys — no hand-edit required. Add a test here only if you want
   to lock in a specific canonical-form shape.

9. **Tests** in the `#[cfg(test)] mod tests` block at the bottom of
   `src/overture/mod.rs`. Add at least one `parse_overture_geojson` test
   using a small fixture GeoJSON string, and at least one `from_str_loose`
   test covering each new alias. Existing tests
   (`base_landuse_forest_subtype`, `from_str_loose_*`) are good templates.

10. **Documentation.** If the theme list appears in user-facing docs, update
    `README.md` (the "Overture Maps" section) and `docs/ARCHITECTURE.md`
    (the Overture integration section).

Modules that **do not** need changes:

- `src/source_options.rs` parsers (`parse_overture_themes`,
  `parse_overture_priority`, `parse_overture_theme_list`) build on
  `OvertureTheme::all()` and `from_str_loose`, so they pick up new variants
  automatically. Add a test there only if you want to lock in a specific alias
  spelling.

Run `make checkall` after the edits. If `cargo doc` fails with a broken
intra-doc link, you likely forgot step 5.

## Cache-Migration Startup Contract

Cache getters in this crate are pure path resolution. They do not migrate
anything. Consumers **must** call `cache::migrate_legacy_caches` once at
startup, before any cache access, to relocate legacy `osm-to-bedrock` cache
directories into the shared default location:

```rust,no_run
fn main() -> anyhow::Result<()> {
    // Call once at startup. Idempotent: a second invocation is a no-op once
    // the legacy directories are empty. Always targets the shared default
    // location; PAR_OSM_*_CACHE_DIR / *_CACHE_DIR override directories are
    // never touched.
    let _report = par_osm_rust::cache::migrate_legacy_caches()?;
    Ok(())
}
```

When you add a new cache path or change the cache layout, keep the getters
side-effect-free. Migration belongs in `cache::migrate_legacy_caches` (or a
helper it calls), not in an accessor. This avoids the first-call race between
concurrent callers and keeps the getters cheap to call repeatedly.

## Related Documentation

- [README](README.md) - Usage, examples, cache behavior, and the verification
  commands.
- [Architecture](docs/ARCHITECTURE.md) - Module boundaries, source flow, cache
  architecture, and the tradeoffs behind them.
- [Documentation Style Guide](docs/DOCUMENTATION_STYLE_GUIDE.md) - Formatting,
  tone, code block conventions, and Mermaid diagram standards for project
  documentation.
- [Changelog](CHANGELOG.md) - Released changes, organized per Keep a
  Changelog. The footer links resolve to commit-range comparisons on GitHub
  (no release tags exist yet — see the README release checklist).
