//! `overturemaps` CLI subprocess invocation and high-level fetch orchestration.
//!
//! The entire submodule is gated behind `#[cfg(feature = "blocking")]` at the
//! `mod cli;` declaration in `super` (ARC-012), so the items in this
//! file do not carry their own per-item `cfg` gates.

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::bbox::BBox;
use crate::osm::OsmData;
// QA-107: truncation helpers consolidated in `crate::text_truncate`.
use crate::text_truncate::{str_prefix_at_boundary, str_suffix_at_boundary};

use super::cache::{
    OvertureParams, overture_cache_dir, overture_cache_key_with_version, overture_cache_read,
    overture_cache_write,
};
use super::parse::parse_overture_geojson_with_allocator;
use super::theme::OvertureTheme;
use crate::synthetic_ids::OvertureIdAllocator;

/// Environment override that selects an absolute path to the `overturemaps`
/// CLI binary, bypassing the default PATH lookup (SEC-010). Must be set to an
/// absolute path that exists on disk; relative paths or missing files fall
/// back to the PATH-based `overturemaps` lookup.
const OVERTURE_CLI_ENV_OVERRIDE: &str = "PAR_OSM_OVERTURE_CLI";

const CLI_CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const CLI_POLL_INTERVAL: Duration = Duration::from_millis(250);

const STDERR_SNIPPET_LIMIT: usize = crate::text_truncate::TRUNCATE_LIMIT;

// ── CLI availability check ────────────────────────────────────────────────

/// Resolve the `overturemaps` CLI binary path (SEC-010).
///
/// Resolution order:
/// 1. `PAR_OSM_OVERTURE_CLI` environment override, if it points to an existing
///    absolute path. Absolute paths are exec'd directly, bypassing PATH lookup
///    (which is vulnerable to binary-hijack in multi-user / shared-PATH
///    setups where another user can shadow `overturemaps` earlier on PATH).
/// 2. Fall back to the bare program name `"overturemaps"`, resolved by the
///    operating system via `PATH` (current default behavior).
///
/// Relative paths and paths to non-existent files silently fall back to the
/// PATH lookup so a misconfigured override never blocks a fetch.
fn resolve_overture_cli() -> PathBuf {
    if let Ok(raw) = std::env::var(OVERTURE_CLI_ENV_OVERRIDE) {
        let path = PathBuf::from(&raw);
        if path.is_absolute() && path.exists() {
            return path;
        }
        log::debug!(
            "PAR_OSM_OVERTURE_CLI='{raw}' is not an absolute path to an existing file; falling back to PATH lookup"
        );
    }
    PathBuf::from("overturemaps")
}

/// Check whether the `overturemaps` CLI is available on the system PATH (or
/// via the `PAR_OSM_OVERTURE_CLI` override — see `resolve_overture_cli`).
///
/// Runs `overturemaps --version` with a short timeout.  Returns `true` if
/// the command succeeds (exit code 0), `false` otherwise.
pub fn is_cli_available() -> bool {
    let Ok(mut child) = std::process::Command::new(resolve_overture_cli())
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };

    // ARC-111 (≈ QA-112): the poll/kill/reap core is shared with the version
    // probe and the main runner via `wait_with_timeout`. The availability
    // check maps any error (try_wait failure or timeout) to `false`.
    match wait_with_timeout(&mut child, CLI_CHECK_TIMEOUT) {
        Ok(status) => status.success(),
        Err(_) => false,
    }
}

/// Best-effort probe of the `overturemaps` CLI version string (ARC-001).
///
/// Spawn-and-poll `overturemaps --version`, capturing stdout. Returns the
/// first non-empty trimmed line of stdout on success. On any failure
/// (spawn error, non-zero exit, timeout, malformed UTF-8), returns
/// `"unknown"` so the caller can still fold *something* into the cache key
/// without blocking the fetch. Cached per-process via [`OnceLock`].
fn probe_cli_version_uncached() -> String {
    let cli = resolve_overture_cli();
    let Ok(mut child) = std::process::Command::new(&cli)
        .arg("--version")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return "unknown".to_string();
    };

    // ARC-111 (≈ QA-112): shared poll/kill/reap core. Any error (try_wait
    // failure or timeout) folds to `"unknown"` so a probe failure never
    // blocks the fetch.
    let exit_status = match wait_with_timeout(&mut child, CLI_CHECK_TIMEOUT) {
        Ok(status) => status,
        Err(_) => return "unknown".to_string(),
    };

    if !exit_status.success() {
        return "unknown".to_string();
    }

    // QA-115 / ARC-111: stdout is read only after the child has exited.
    // `--version` output is a short banner (well under 1 KiB), so it fits
    // comfortably inside the OS pipe buffer (~64 KiB on Linux/macOS) and the
    // writer side cannot deadlock against this reader. A larger stream read
    // this way before exit WOULD deadlock (writer blocks once the pipe
    // fills, child never reaches exit, we never reach `read_to_string`) —
    // for that shape use the main runner's stderr-file pattern instead.
    let Some(mut stdout) = child.stdout.take() else {
        return "unknown".to_string();
    };
    let mut buf = String::new();
    if stdout.read_to_string(&mut buf).is_err() {
        return "unknown".to_string();
    }
    buf.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

/// Process-wide cached `overturemaps` CLI version. Probed once on first use
/// (ARC-001); subsequent calls return the cached value without re-shelling
/// out. Probing failure yields `"unknown"`.
fn cached_cli_version() -> &'static str {
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED.get_or_init(probe_cli_version_uncached)
}

fn stderr_suffix(stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        String::new()
    } else if stderr.len() <= STDERR_SNIPPET_LIMIT {
        format!(": {stderr}")
    } else {
        let head_len = STDERR_SNIPPET_LIMIT / 2;
        let tail_len = STDERR_SNIPPET_LIMIT - head_len;
        let head = str_prefix_at_boundary(stderr, head_len);
        let tail = str_suffix_at_boundary(stderr, tail_len);
        let omitted = stderr.len().saturating_sub(head.len() + tail.len());
        format!(": {head}\n...[stderr truncated, {omitted} bytes omitted]...\n{tail}")
    }
}

fn read_stderr_file(stderr_path: &Path, cli_type: &str) -> Result<Vec<u8>> {
    std::fs::read(stderr_path)
        .with_context(|| format!("reading overturemaps stderr for type '{cli_type}'"))
}

/// Poll `child` for completion with [`CLI_POLL_INTERVAL`] cadence until it
/// exits naturally or `timeout` elapses (ARC-111 ≈ QA-112).
///
/// Returns `Ok(status)` only when the child exited on its own. On timeout
/// the child is killed and reaped, then `Err` is returned; a `try_wait`
/// failure also returns `Err`. The three CLI subprocess sites in this
/// module used to duplicate this poll/kill/reap core; they layer their own
/// return shapes on top of this primitive:
///
/// * [`is_cli_available`] maps `Err` to `false`.
/// * [`probe_cli_version_uncached`] maps `Err` to `"unknown"`.
/// * [`wait_with_stderr_file_timeout`] (the main runner) maps `Err` to a
///   stderr-contextualized timeout bail. The stderr-file plumbing stays at
///   that call site — only the poll/kill/reap core is shared here.
fn wait_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<std::process::ExitStatus> {
    let start = Instant::now();
    loop {
        match child.try_wait().context("polling overturemaps CLI")? {
            Some(status) => return Ok(status),
            None => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    child
                        .wait()
                        .context("waiting for overturemaps CLI after timeout")?;
                    bail!("overturemaps CLI timed out");
                }
                std::thread::sleep(CLI_POLL_INTERVAL);
            }
        }
    }
}

fn wait_with_stderr_file_timeout(
    mut child: std::process::Child,
    stderr_path: &Path,
    timeout: Duration,
    timeout_secs: u64,
    cli_type: &str,
) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    // ARC-111 (≈ QA-112): delegate the poll/kill/reap core to
    // `wait_with_timeout`; keep the stderr-file read + the rich timeout
    // message at this call site so the timeout error text is preserved
    // exactly (the main runner streams stderr to a file, which the shared
    // helper does not know about).
    match wait_with_timeout(&mut child, timeout) {
        Ok(status) => {
            let stderr = read_stderr_file(stderr_path, cli_type)?;
            Ok((status, stderr))
        }
        Err(_) => {
            // `wait_with_timeout` already killed and reaped the child.
            let stderr = read_stderr_file(stderr_path, cli_type)?;
            let stderr_msg = stderr_suffix(&stderr);
            bail!(
                "overturemaps CLI timed out after {timeout_secs}s for type '{cli_type}'{stderr_msg}"
            );
        }
    }
}

// ── GeoJSON download via CLI ──────────────────────────────────────────────

/// Validate a `cli_type` string before passing it to the `overturemaps` CLI
/// (SEC-012).
///
/// The crate's own use is safe (themes come from [`OvertureTheme::cli_types`]),
/// but [`fetch_geojson_for_type`] is `pub` and an external caller could pass
/// user input. A `cli_type` containing `-` or whitespace enables argument
/// injection against the CLI's arg parser (e.g. `--output=...` would be
/// honored as a flag, not a positional `--type` value). Reject those shapes
/// at the public boundary; the shell-out itself is already arg-vector based
/// (no shell), so this is the only remaining injection vector.
///
/// Visible to `super::tests` (`pub(super)`) so the SEC-012 guard unit test in
/// `mod.rs` can call it without widening the public API.
pub(super) fn validate_cli_type(cli_type: &str) -> Result<()> {
    if cli_type.is_empty() {
        bail!("overturemaps cli_type must not be empty");
    }
    if cli_type.contains('-') || cli_type.chars().any(char::is_whitespace) {
        bail!(
            "overturemaps cli_type '{cli_type}' rejected: contains '-' or whitespace \
             (argument-injection guard, SEC-012)"
        );
    }
    Ok(())
}

/// Download Overture GeoJSON for a single CLI type and bounding box.
///
/// Honors the `PAR_OSM_OVERTURE_CLI` environment override for the binary
/// path (see `resolve_overture_cli`, SEC-010). The shell-out is
/// arg-vector based (no shell); `cli_type` is validated by
/// `validate_cli_type` to reject argument-injection attempts (SEC-012).
///
/// Invokes:
/// ```text
/// overturemaps download --bbox W,S,E,N -t <cli_type> -o <tmpfile>
/// ```
///
/// # Arguments
///
/// * `cli_type` – The Overture type string (e.g. `"building"`, `"segment"`).
///   Must not contain `-` or whitespace.
/// * `bbox` – `(south, west, north, east)` bounding box.
/// * `timeout_secs` – Maximum wall-clock seconds to wait for the CLI.
///
/// # Returns
///
/// The GeoJSON string written by the CLI, or an error if the CLI fails or
/// times out.
pub fn fetch_geojson_for_type(cli_type: &str, bbox: &BBox, timeout_secs: u64) -> Result<String> {
    validate_cli_type(cli_type)?;

    // ARC-106: Overture CLI expects WSEN order — `BBox::wsen()` is the
    // boundary adapter that produces it from the crate's SWNE storage.
    let (min_lon, min_lat, max_lon, max_lat) = bbox.wsen();
    let bbox_str = format!("{min_lon},{min_lat},{max_lon},{max_lat}");

    // Write output to a named temp file so the CLI can stream to disk.
    let tmp = tempfile::Builder::new()
        .suffix(".geojson")
        .tempfile()
        .context("creating temp file for overturemaps output")?;
    let tmp_path = tmp.path().to_path_buf();

    let stderr_tmp = tempfile::Builder::new()
        .suffix(".stderr")
        .tempfile()
        .context("creating temp file for overturemaps stderr")?;
    let stderr_path = stderr_tmp.path().to_path_buf();
    let stderr_file = stderr_tmp
        .reopen()
        .context("opening temp file for overturemaps stderr")?;

    let cli_path = resolve_overture_cli();
    let child = std::process::Command::new(&cli_path)
        .arg("download")
        .arg("-f")
        .arg("geojson")
        .arg("--bbox")
        .arg(&bbox_str)
        .arg("-t")
        .arg(cli_type)
        .arg("-o")
        .arg(&tmp_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()
        .with_context(|| format!("spawning overturemaps CLI at {}", cli_path.display()))?;

    let (status, stderr) = wait_with_stderr_file_timeout(
        child,
        &stderr_path,
        Duration::from_secs(timeout_secs),
        timeout_secs,
        cli_type,
    )?;

    if !status.success() {
        let stderr_msg = stderr_suffix(&stderr);
        bail!(
            "overturemaps CLI exited with status {} for type '{cli_type}'{stderr_msg}",
            status.code().unwrap_or(-1)
        );
    }

    let content = std::fs::read_to_string(&tmp_path)
        .with_context(|| format!("reading overturemaps output for type '{cli_type}'"))?;

    Ok(content)
}

// ── High-level fetch API ──────────────────────────────────────────────────

/// Create an empty [`OsmData`] to accumulate merged results into.
fn empty_osm_data() -> OsmData {
    OsmData::default()
}

/// Fetch + cache + parse a single Overture theme/CLI-type pair (QA-007).
///
/// Shared by [`fetch_overture_data`] (which propagates the error) and
/// [`fetch_overture_data_best_effort`] (which logs and skips). Returns the
/// parsed [`OsmData`] for the single theme. Cache key is version-aware
/// ([`overture_cache_key_with_version`]) and reads enforce
/// [`OvertureParams::cache_ttl`] (ARC-001).
///
/// `id_alloc` is threaded through [`parse_overture_geojson_with_allocator`]
/// so a single fetch that merges multiple themes never mints colliding
/// synthetic IDs across themes (ARC-101). The caller owns the allocator and
/// passes the same `&mut` to every per-theme call in one fetch.
fn fetch_one_theme(
    theme: OvertureTheme,
    cli_type: &'static str,
    bbox: &BBox,
    params: &OvertureParams,
    cache_dir: &Path,
    cli_version: &str,
    id_alloc: &mut OvertureIdAllocator,
) -> Result<OsmData> {
    let key = overture_cache_key_with_version(bbox, cli_type, cli_version);
    let ttl = params.cache_ttl();

    let geojson = match overture_cache_read(cache_dir, &key, Some(ttl)) {
        Some(cached) => {
            log::debug!("Overture cache hit for {cli_type} (key {key})");
            cached
        }
        None => {
            log::debug!("Overture cache miss for {cli_type} — downloading");
            let fetched = fetch_geojson_for_type(cli_type, bbox, params.timeout_secs)
                .with_context(|| format!("fetching Overture data for type '{cli_type}'"))?;
            // QA-116: the write happens unconditionally, even when
            // `params.cache_ttl()` is zero (which only disables read-back).
            // Documented behavior — useful for refresh-only flows; a future
            // release may skip writes on zero TTL.
            overture_cache_write(cache_dir, &key, bbox, cli_type, cli_version, &fetched)
                .with_context(|| format!("caching Overture data for type '{cli_type}'"))?;
            fetched
        }
    };

    parse_overture_geojson_with_allocator(&geojson, theme, id_alloc)
        .with_context(|| format!("parsing Overture GeoJSON for type '{cli_type}'"))
}

/// Fetch Overture Maps data for the enabled themes in `params` and normalize
/// it into a single [`OsmData`] (DOC-011: removed the duplicated summary that
/// previously appeared around the `# Errors` section).
///
/// For each CLI type belonging to each requested theme:
/// 1. Check the disk cache (version-aware key + TTL, ARC-001).
/// 2. On cache miss, invoke the `overturemaps` CLI to download GeoJSON.
/// 3. Write the result to cache with the probed CLI version.
/// 4. Parse the GeoJSON into [`OsmData`] and merge.
///
/// This function shells out to the optional `overturemaps` CLI and may perform
/// network I/O. The returned data can be merged with OSM data via
/// [`crate::sources::merge_source_data`] or fetched through the higher-level
/// [`crate::sources::fetch_map_data`] orchestrator.
///
/// # Errors
///
/// Returns an error if `params.enabled` is false, the CLI is not installed,
/// or any theme fetch or parse fails.
///
/// # Examples
///
/// ```no_run
/// # #[cfg(feature = "blocking")] fn main() -> anyhow::Result<()> {
/// use par_osm_rust::bbox::BBox;
/// use par_osm_rust::overture::{fetch_overture_data, OvertureParams, OvertureTheme};
///
/// let bbox = BBox::new(38.0, -121.0, 38.01, -120.99)?; // south, west, north, east
/// let params = OvertureParams {
///     enabled: true,
///     themes: vec![OvertureTheme::Place],
///     ..Default::default()
/// };
/// let mut progress = |_: f32, _: &str| {};
/// let data = fetch_overture_data(&bbox, &params, &mut progress)?;
/// println!("{} ways", data.iter_ways().count());
/// # Ok(())
/// # }
/// # #[cfg(not(feature = "blocking"))] fn main() {}
/// ```
pub fn fetch_overture_data(
    bbox: &BBox,
    params: &OvertureParams,
    progress_cb: crate::ProgressFn<'_>,
) -> Result<OsmData> {
    fetch_overture_with_policy(bbox, params, progress_cb, FailurePolicy::FailFast)
}

/// Like [`fetch_overture_data`] but never fails.
///
/// - If Overture is disabled, returns empty [`OsmData`].
/// - If the CLI is unavailable, returns empty [`OsmData`] after logging a warning.
/// - If a theme fetch fails, logs a warning and skips it.
/// - If parsing a GeoJSON result fails, logs a warning and skips it.
///
/// Use this lower-level helper when callers want partial Overture data without
/// bubbling errors. Applications that need explicit fallback status should prefer
/// [`crate::sources::fetch_map_data`].
pub fn fetch_overture_data_best_effort(
    bbox: &BBox,
    params: &OvertureParams,
    progress_cb: crate::ProgressFn<'_>,
) -> OsmData {
    match fetch_overture_with_policy(bbox, params, progress_cb, FailurePolicy::BestEffort) {
        Ok(data) => data,
        // BestEffort never produces an `Err` at the policy branches (disabled
        // and CLI-unavailable both return `Ok(empty)`); the match arm exists
        // only to satisfy the type system. If a per-theme error escaped the
        // loop somehow, treat it as a skip and return whatever accumulated.
        Err(_) => empty_osm_data(),
    }
}

/// Per-theme error-handling policy for [`fetch_overture_with_policy`]
/// (ARC-111 ≈ QA-112).
///
/// [`fetch_overture_data`] and [`fetch_overture_data_best_effort`] share
/// every other step of the fetch/parse/merge/progress loop; the only axis
/// of variation is what happens when a per-theme fetch or parse fails.
/// This enum captures that single axis so the orchestration lives in
/// exactly one place and the two public entry points cannot drift apart
/// (the prior implementation required dual maintenance for every
/// ARC-101-style change).
enum FailurePolicy {
    /// Propagate the first per-theme error via `?`, aborting the fetch.
    FailFast,
    /// Log the per-theme error and continue with the remaining themes.
    BestEffort,
}

/// Shared per-cli_type fetch/parse/merge/progress loop for the Overture CLI
/// orchestrator (ARC-111 ≈ QA-112).
///
/// Everything other than per-theme error handling is identical between
/// [`fetch_overture_data`] and [`fetch_overture_data_best_effort`] and lives
/// here exactly once: the `enabled` / CLI-availability guards, the version
/// probe, the (theme, cli_type) flattening, the progress callback fractions,
/// the per-fetch [`OvertureIdAllocator`] (ARC-101), the per-theme cache hit /
/// miss + parse via [`fetch_one_theme`], the final stats log, and the
/// trailing `1.0` progress ping.
fn fetch_overture_with_policy(
    bbox: &BBox,
    params: &OvertureParams,
    progress_cb: crate::ProgressFn<'_>,
    policy: FailurePolicy,
) -> Result<OsmData> {
    if !params.enabled {
        return match policy {
            FailurePolicy::FailFast => bail!("Overture Maps integration is not enabled"),
            FailurePolicy::BestEffort => Ok(empty_osm_data()),
        };
    }
    if !is_cli_available() {
        return match policy {
            FailurePolicy::FailFast => bail!(
                "The `overturemaps` CLI is not installed.\n\
                 Install it with: pip install overturemaps\n\
                 Then retry."
            ),
            FailurePolicy::BestEffort => {
                log::warn!(
                    "Overture Maps CLI not available — skipping Overture data.\n\
                     Install with: pip install overturemaps"
                );
                Ok(empty_osm_data())
            }
        };
    }

    let cli_version = cached_cli_version();
    let theme_names: Vec<String> = params.themes.iter().map(|t| t.to_string()).collect();
    log::info!(
        "Starting Overture Maps fetch (bbox: {:.4},{:.4},{:.4},{:.4}, themes: {}, cli_version: {})",
        bbox.south,
        bbox.west,
        bbox.north,
        bbox.east,
        theme_names.join(", "),
        cli_version,
    );

    let cache_dir = overture_cache_dir();

    // Flatten all (theme, cli_type) pairs so progress can be reported as a
    // fraction of total work.
    let pairs: Vec<(OvertureTheme, &'static str)> = params
        .themes
        .iter()
        .flat_map(|&theme| theme.cli_types().into_iter().map(move |t| (theme, t)))
        .collect();

    let total = pairs.len() as f32;
    let mut accumulated = empty_osm_data();

    // One allocator per fetch (ARC-101): threading the same `&mut` through
    // every per-theme parse guarantees unique synthetic IDs across themes,
    // so `merge` cannot violate the `ways` / `ways_by_id` invariant on the
    // accumulated result. Per-fetch determinism (identical inputs → identical
    // IDs) is preserved because every fetch starts a fresh allocator.
    let mut id_alloc = OvertureIdAllocator::new();

    for (i, (theme, cli_type)) in pairs.iter().enumerate() {
        let pct = i as f32 / total;
        progress_cb(pct, &format!("Fetching Overture {cli_type}…"));

        match fetch_one_theme(
            *theme,
            cli_type,
            bbox,
            params,
            &cache_dir,
            cli_version,
            &mut id_alloc,
        ) {
            Ok(data) => accumulated.merge(data),
            Err(e) => match policy {
                FailurePolicy::FailFast => return Err(e),
                FailurePolicy::BestEffort => {
                    log::warn!("Skipping Overture type '{cli_type}': {e}");
                }
            },
        }
    }

    // The completion stats log fires only in FailFast mode to preserve the
    // observable behavior of the two prior implementations (the best-effort
    // variant never emitted it). Both modes ping progress at 1.0.
    if matches!(policy, FailurePolicy::FailFast) {
        log::info!(
            "Overture Maps fetch complete ({} ways, {} POI nodes, {} address nodes)",
            accumulated.ways().len(),
            accumulated.poi_nodes.len(),
            accumulated.addr_nodes.len(),
        );
    }
    progress_cb(1.0, "Overture data ready");
    Ok(accumulated)
}
