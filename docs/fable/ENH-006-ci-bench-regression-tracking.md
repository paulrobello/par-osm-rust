# ENH-006 — Criterion Benchmark Regression Tracking in CI

> Status: proposed · Effort: Medium (~1 day incl. tuning) · Impact: perf regressions surface at PR time
> **CI change — maintainer review required before merging (repo policy: CI/security
> changes are opt-in).** Independent of audit phases.

## Goal

Run the three criterion benches on pull requests and compare against the base branch
automatically, so regressions on the protected hot paths (XML parse, XML write,
source merge) become visible review information instead of relying on a developer
remembering to run `cargo bench` and eyeball deltas.

## Current State (verified at commit `55187f4`)

- Benches: `benches/parse_osm_xml.rs`, `benches/write_osm_xml.rs`,
  `benches/merge_source_data.rs` — criterion 0.5, `harness = false`, each with
  correctness guard assertions inside the timed closures and construction baselines.
- CI (`.github/workflows/ci.yml`): 3-OS test matrix, MSRV, docs, audit, markdown lint
  jobs — no bench job. Actions currently tag-pinned (SEC-106 will move to SHA pins —
  land that first or pin the new job's actions the same way SEC-106 does).
- No bench baselines are stored anywhere; comparisons are manual.

## Design

Use criterion's built-in baseline mechanism + [`critcmp`](https://github.com/BurntSushi/critcmp):
run benches on the PR's **base** commit with `--save-baseline base`, then on the PR
head with `--save-baseline head`, then `critcmp base head` and post/emit the table.
All within one job on one runner so machine variance cancels out. The job is
**informational** (`continue-on-error: true`) — GitHub-hosted runner noise makes hard
gating counterproductive; the signal is the printed comparison, with a soft threshold
highlighted.

## Implementation Steps

1. Add a new job to `.github/workflows/ci.yml` (or a separate `bench.yml` triggered on
   `pull_request` only — prefer separate file so `ci.yml` stays required-green):

   ```yaml
   bench-compare:
     name: Bench regression check (informational)
     runs-on: ubuntu-latest
     continue-on-error: true
     if: github.event_name == 'pull_request'
     steps:
       - uses: actions/checkout@<sha>            # pin per SEC-106 convention
         with: { fetch-depth: 0 }
       - uses: dtolnay/rust-toolchain@<sha>
         with: { toolchain: stable }
       - uses: Swatinem/rust-cache@<sha>
       - uses: taiki-e/install-action@<sha>      # already used by the audit job
         with: { tool: critcmp }
       - name: Bench base
         run: |
           git checkout ${{ github.event.pull_request.base.sha }}
           cargo bench --bench parse_osm_xml --bench write_osm_xml --bench merge_source_data -- --save-baseline base --warm-up-time 1 --measurement-time 3
       - name: Bench head
         run: |
           git checkout ${{ github.event.pull_request.head.sha }}
           cargo bench --bench parse_osm_xml --bench write_osm_xml --bench merge_source_data -- --save-baseline head --warm-up-time 1 --measurement-time 3
       - name: Compare
         run: |
           critcmp base head | tee bench-compare.txt
           echo '## Bench comparison (base → head)' >> "$GITHUB_STEP_SUMMARY"
           echo '```' >> "$GITHUB_STEP_SUMMARY"
           cat bench-compare.txt >> "$GITHUB_STEP_SUMMARY"
           echo '```' >> "$GITHUB_STEP_SUMMARY"
   ```

   ⚠️ Verify the criterion CLI flags against criterion 0.5's actual arg parsing
   (`cargo bench -- --help`) — `--save-baseline`, `--warm-up-time`, `--measurement-time`
   are criterion args and must come after the `--` separator, and apply per-bench-binary.
   ⚠️ `git checkout <sha>` inside the job: the `Cargo.lock`-less repo resolves deps at
   each checkout — acceptable; if SEC-107 lands (committed lockfile), nothing changes.
2. Soft threshold: append a step that greps the critcmp output for regressions >10%
   and, if found, adds a `⚠️ possible regression` line to the step summary (do NOT
   fail the job — `continue-on-error` already guards, but keep exit 0 explicitly).
3. Wall-clock control: with `--warm-up-time 1 --measurement-time 3` the three benches
   run in a few minutes. If job time exceeds ~10 min, reduce criterion sample size via
   the same CLI (`--sample-size 30`) — tune once, note the chosen numbers in the
   workflow file as comments.
4. Docs: add a short "Benchmarks in CI" paragraph to CONTRIBUTING.md's testing section
   (post-DOC-001 tree) describing how to read the comparison and the local equivalent
   (`cargo bench -- --save-baseline main`, hack, `critcmp main new`).
5. Present the whole workflow diff to the maintainer for review before merge (CI
   policy). Pin all action SHAs consistently with SEC-106's outcome.

## Files to Touch

- `.github/workflows/bench.yml` (new; or a job appended to `ci.yml`)
- `CONTRIBUTING.md` (one paragraph)

## Verification

- Open a test PR with a deliberate regression (e.g. add `std::thread::sleep(1ms)`
  inside a bench's timed closure on a scratch branch) — the summary table must show
  the regression; then close the PR and confirm a no-change PR reads ~1.00 ratios.
- `actionlint .github/workflows/bench.yml` locally if available.

## Rollback

Delete the workflow file (or job). No library code involved. Baselines live only in
the runner's target dir — nothing persisted to clean up.
