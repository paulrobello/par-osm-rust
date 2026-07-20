# ENH-001 — Parallel SRTM Tile Downloads

> Status: done (commit `df2eed9`, 2026-07-19) · Effort: Medium (~1 day) · Impact: 3–5× faster multi-tile elevation fetches
> Prerequisite: apply AUDIT Phase 1 fixes to `src/srtm.rs` first (SEC-101, SEC-102, SEC-108) —
> they rewrite `download_tile` internals and add bbox validation this plan builds on.

## Goal

Overlap SRTM tile downloads with a small bounded worker pool so a multi-tile bbox is
network-bound on N connections instead of one, without adding an async runtime, without
changing the public API, and preserving the existing retry, error-aggregation, and
progress-callback semantics.

## Current State (verified at commit `55187f4`)

- `download_tiles_for_bbox` (`src/srtm.rs:195-240`) iterates `tiles_for_bbox(...)`
  sequentially; each iteration calls `progress_cb(i, total, &name)` then
  `download_tile_with_retry(lat, lon, dest_dir, 3)`.
- `download_tile_with_retry` (`src/srtm.rs:160-182`) sleeps inline (exponential backoff
  1 s, 2 s) between attempts — during a retry sleep, nothing else downloads.
- `download_tile` (`src/srtm.rs:96-154`) uses `shared_client()` — a module
  `OnceLock<reqwest::blocking::Client>`; `reqwest::blocking::Client` is `Send + Sync`
  and internally pooled, so concurrent use from threads is safe and already the
  crate's pattern.
- The progress callback type is `&dyn Fn(usize, usize, &str)` — NOT `Sync`, so it must
  only ever be invoked from the calling thread.
- Failures are collected into `failed: Vec<String>` and reported in one aggregate
  `bail!` after the loop; per-tile skip (`Ok(false)`) means the `.hgt` already existed.

## Implementation Steps

1. Read the current `src/srtm.rs` in full (Phase-1 remediation will have changed it).
2. Add near the other module constants:

   ```rust
   /// Number of concurrent SRTM tile downloads. Bounded to stay polite to the
   /// public tile bucket; raise cautiously.
   const SRTM_DOWNLOAD_CONCURRENCY: usize = 4;
   ```

3. Rewrite the loop body of `download_tiles_for_bbox` using scoped threads and a
   channel back to the caller thread (callback stays on the caller thread — it is not
   `Sync` and must not move):

   ```rust
   use std::sync::atomic::{AtomicUsize, Ordering};
   use std::sync::mpsc;

   enum TileEvent { Started(usize, String), Done(String, Result<bool>) }

   let next = AtomicUsize::new(0);
   let (tx, rx) = mpsc::channel::<TileEvent>();
   let workers = SRTM_DOWNLOAD_CONCURRENCY.min(total).max(1);

   std::thread::scope(|s| {
       for _ in 0..workers {
           let tx = tx.clone();
           let tiles = &tiles;
           s.spawn(move || loop {
               let i = next.fetch_add(1, Ordering::Relaxed);
               let Some((lat, lon)) = tiles.get(i).copied() else { break };
               let name = tile_name(lat, lon);
               let _ = tx.send(TileEvent::Started(i, name.clone()));
               let res = download_tile_with_retry(lat, lon, dest_dir, 3);
               let _ = tx.send(TileEvent::Done(name, res));
           });
       }
       drop(tx); // scope's own sender — rx ends when all workers finish

       // Drain on the caller thread: this is where progress_cb is invoked.
       let mut completed = 0usize;
       for ev in rx {
           match ev {
               TileEvent::Started(i, name) => progress_cb(i, total, &name),
               TileEvent::Done(name, Ok(true)) => { downloaded += 1; completed += 1; }
               TileEvent::Done(_, Ok(false)) => { completed += 1; }
               TileEvent::Done(name, Err(e)) => {
                   log::error!("Elevation tile {name} could not be downloaded: {e:#}");
                   failed.push(name);
                   completed += 1;
               }
           }
       }
       debug_assert_eq!(completed, total);
   });
   ```

   Keep the existing pre-loop logging, the `total == 0` early return, the aggregate
   `bail!` on non-empty `failed`, and the final success log exactly as they are.
4. Update the function's rustdoc: progress callbacks may now arrive out of tile order
   (the `tile_index` argument still identifies which tile started); tiles download up
   to `SRTM_DOWNLOAD_CONCURRENCY` at a time; retry backoff applies per tile and
   overlaps other tiles' downloads.
5. Concurrency-safety note to verify while editing: after the Phase-1 SEC-108 fix,
   `download_tile` writes via `tempfile::NamedTempFile` (unique names), so two workers
   racing on the *same* tile cannot corrupt files — but the work distribution above
   assigns each tile exactly once, so that race only matters across processes (already
   handled). Do NOT reintroduce a shared fixed `.tmp` path.
6. Tests (no network; test the orchestration):
   - Existing `tiles_for_bbox` tests unchanged.
   - The channel/worker logic is exercised by the existing offline behavior:
     `download_tiles_for_bbox` over a bbox whose tiles ALL already exist in a tempdir
     (create empty `.hgt` files first) must return `Ok(0)`, invoke the callback once
     per tile with the correct `total`, and leave files untouched. Add this test if a
     variant does not already exist; it runs the full concurrent path without network.
   - A mixed test: pre-create some tiles, point `dest_dir` at a tempdir, and use an
     unreachable `OVERPASS`-style env override? — NOT possible here (SRTM URL is a
     hardcoded const), so missing tiles would hit the network: keep network cases out;
     rely on the all-cached test plus unit-testing `TileEvent` accounting if extracted.
7. Run the module's tests plus the full gate.

## Files to Touch

- `src/srtm.rs` — the loop rewrite, constant, rustdoc, one new test.

## Verification

```bash
cargo test srtm
make checkall
cargo test --no-default-features   # srtm is blocking-gated; confirm the pure build is untouched
```

Manual (optional, network): in a scratch program or `examples/`, fetch a 2×2-degree
bbox against a tempdir twice — first run downloads concurrently (observe interleaved
`Downloading elevation tile …` logs), second run returns `Ok(0)` fast.

## Rollback

Single-file change; `git checkout -- src/srtm.rs` (or revert the commit). The public
signature is unchanged, so no downstream impact. If the tile bucket ever throttles
concurrent connections, set `SRTM_DOWNLOAD_CONCURRENCY` back to 1 — that restores
strictly sequential behavior through the same code path.
