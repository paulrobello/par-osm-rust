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

`--all-features` matters: this crate has no optional features today, but CI
must remain green against future feature-gated code paths. A PR that passes
`cargo test` but fails `cargo test --all-features` is not ready to merge.

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
docs(audit): add comprehensive project audit (AUDIT.md)
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
  does not yet enforce `#![warn(missing_docs)]`, but `src/sources.rs` already
  meets the bar so new code should not regress it.
- Include `# Errors` on any function returning `Result`, explaining when and
  why an error is returned.
- Include `# Panics` on any function that can panic, with the precondition
  that would prevent it.
- Cross-reference other items with intra-doc links (`[\`SourceOptions\`]`,
  `[\`fetch_map_data\`]`) so rustdoc renders them as links.
- Mark example blocks with ` ```no_run ` when they touch the network or the
  filesystem, so `cargo test --doc` does not attempt to execute them.

`cargo doc --no-deps -D rustdoc::broken_intra_doc_links` is part of the
documentation CI job. A broken intra-doc link fails the build.

## Testing Conventions

Tests are inline. Each module ends with a `#[cfg(test)] mod tests` block
holding its own fixtures and assertions — there is no top-level `tests/`
directory. Put new tests in the module that owns the code under test.

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
passes with it. The audit tracks uncovered areas explicitly (real PBF parsing,
`write_osm_xml_string` round-trip, `clip_to_bbox`); contributions that close
those gaps are welcome.

## Walkthrough: Adding a New OvertureTheme Variant

This walkthrough enumerates every spot that must change when adding a new
`OvertureTheme` variant. It is the canonical example of a cross-cutting change
in this crate, because themes touch the enum, its mappings, parsing, the
tag-mapping layer, the feature-routing layer, and tests.

Suppose you are adding a new theme, `OvertureTheme::Water` (used here purely
as an example; the real `Base` theme already covers water). The full set of
edits lives in `src/overture.rs` unless noted.

1. **The enum itself.** Add the variant to `pub enum OvertureTheme` with a
   rustdoc comment describing what the theme represents.

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

5. **`impl Display for OvertureTheme`.** Add the canonical lowercase string
   for the new variant. This is used in logs, warnings, and cache keys.

6. **`map_tags_for_theme()`.** Add a match arm that converts Overture feature
   properties into OSM tags for the new theme. This is where Overture's
   property schema becomes OSM-flavored tags that downstream consumers can
   render.

7. **`parse_overture_geojson()`.** Add a match arm in the feature-routing
   branch that decides which `OsmData` collection a feature of the new theme
   lands in (`poi_nodes`, `addr_nodes`, or a way collection). For example,
   `Place` routes to `poi_nodes` and `Address` routes to `addr_nodes`.

8. **Tests at the bottom of `src/overture.rs`.** Add at least one
   `parse_overture_geojson` test using a small fixture GeoJSON string, and at
   least one `from_str_loose` test covering each new alias. Existing tests
   (`base_landuse_forest_subtype`, `from_str_loose_*`) are good templates.

9. **Documentation.** If the theme list appears in user-facing docs, update
   `README.md` (the "Overture Maps" section) and `docs/ARCHITECTURE.md` (the
   Overture integration section).

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
  Changelog.
