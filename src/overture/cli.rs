//! `overturemaps` CLI subprocess invocation and high-level fetch orchestration.
//!
//! The entire submodule is gated behind `#[cfg(feature = "blocking")]` at the
//! `mod cli;` declaration in [`super::mod`] (ARC-012), so the items in this
//! file do not carry their own per-item `cfg` gates.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::osm::OsmData;

use super::cache::{
    OvertureParams, overture_cache_dir, overture_cache_key_with_version, overture_cache_read,
    overture_cache_write,
};
use super::parse::parse_overture_geojson;
use super::theme::OvertureTheme;

/// Environment override that selects an absolute path to the `overturemaps`
/// CLI binary, bypassing the default PATH lookup (SEC-010). Must be set to an
/// absolute path that exists on disk; relative paths or missing files fall
/// back to the PATH-based `overturemaps` lookup.
const OVERTURE_CLI_ENV_OVERRIDE: &str = "PAR_OSM_OVERTURE_CLI";

const CLI_CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const CLI_POLL_INTERVAL: Duration = Duration::from_millis(250);

const STDERR_SNIPPET_LIMIT: usize = 4096;

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

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if start.elapsed() >= CLI_CHECK_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(CLI_POLL_INTERVAL);
            }
            Err(_) => return false,
        }
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

    let start = Instant::now();
    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= CLI_CHECK_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return "unknown".to_string();
                }
                std::thread::sleep(CLI_POLL_INTERVAL);
            }
            Err(_) => return "unknown".to_string(),
        }
    };

    if !exit_status.success() {
        return "unknown".to_string();
    }

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

fn str_prefix_at_boundary(s: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn str_suffix_at_boundary(s: &str, max_bytes: usize) -> &str {
    let mut start = s.len().saturating_sub(max_bytes);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

fn read_stderr_file(stderr_path: &Path, cli_type: &str) -> Result<Vec<u8>> {
    std::fs::read(stderr_path)
        .with_context(|| format!("reading overturemaps stderr for type '{cli_type}'"))
}

fn wait_with_stderr_file_timeout(
    mut child: std::process::Child,
    stderr_path: &Path,
    timeout: Duration,
    timeout_secs: u64,
    cli_type: &str,
) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    let start = Instant::now();
    loop {
        match child.try_wait().context("polling overturemaps CLI")? {
            Some(status) => {
                let stderr = read_stderr_file(stderr_path, cli_type)?;
                return Ok((status, stderr));
            }
            None => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    child
                        .wait()
                        .context("waiting for overturemaps CLI after timeout")?;
                    let stderr = read_stderr_file(stderr_path, cli_type)?;
                    let stderr_msg = stderr_suffix(&stderr);
                    bail!(
                        "overturemaps CLI timed out after {timeout_secs}s for type '{cli_type}'{stderr_msg}"
                    );
                }
                std::thread::sleep(CLI_POLL_INTERVAL);
            }
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
/// * `bbox` – `(min_lat, min_lon, max_lat, max_lon)` bounding box.
/// * `timeout_secs` – Maximum wall-clock seconds to wait for the CLI.
///
/// # Returns
///
/// The GeoJSON string written by the CLI, or an error if the CLI fails or
/// times out.
pub fn fetch_geojson_for_type(
    cli_type: &str,
    bbox: (f64, f64, f64, f64),
    timeout_secs: u64,
) -> Result<String> {
    validate_cli_type(cli_type)?;

    let (min_lat, min_lon, max_lat, max_lon) = bbox;
    // Overture CLI expects W,S,E,N order (min_lon, min_lat, max_lon, max_lat).
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
    OsmData::new(
        HashMap::new(),
        Vec::new(),
        Vec::new(),
        None,
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
}

/// Fetch + cache + parse a single Overture theme/CLI-type pair (QA-007).
///
/// Shared by [`fetch_overture_data`] (which propagates the error) and
/// [`fetch_overture_data_best_effort`] (which logs and skips). Returns the
/// parsed [`OsmData`] for the single theme. Cache key is version-aware
/// ([`overture_cache_key_with_version`]) and reads enforce
/// [`OvertureParams::cache_ttl`] (ARC-001).
fn fetch_one_theme(
    theme: OvertureTheme,
    cli_type: &'static str,
    bbox: (f64, f64, f64, f64),
    params: &OvertureParams,
    cache_dir: &Path,
    cli_version: &str,
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
            overture_cache_write(cache_dir, &key, bbox, cli_type, cli_version, &fetched)
                .with_context(|| format!("caching Overture data for type '{cli_type}'"))?;
            fetched
        }
    };

    parse_overture_geojson(&geojson, theme)
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
/// use par_osm_rust::overture::{fetch_overture_data, OvertureParams, OvertureTheme};
///
/// let bbox = (38.0, -121.0, 38.01, -120.99); // south, west, north, east
/// let params = OvertureParams {
///     enabled: true,
///     themes: vec![OvertureTheme::Place],
///     ..Default::default()
/// };
/// let mut progress = |_: f32, _: &str| {};
/// let data = fetch_overture_data(bbox, &params, &mut progress)?;
/// println!("{} ways", data.iter_ways().count());
/// # Ok(())
/// # }
/// # #[cfg(not(feature = "blocking"))] fn main() {}
/// ```
pub fn fetch_overture_data(
    bbox: (f64, f64, f64, f64),
    params: &OvertureParams,
    progress_cb: &mut dyn FnMut(f32, &str),
) -> Result<OsmData> {
    if !params.enabled {
        bail!("Overture Maps integration is not enabled");
    }
    if !is_cli_available() {
        bail!(
            "The `overturemaps` CLI is not installed.\n\
             Install it with: pip install overturemaps\n\
             Then retry."
        );
    }

    let cli_version = cached_cli_version();
    let theme_names: Vec<String> = params.themes.iter().map(|t| t.to_string()).collect();
    log::info!(
        "Starting Overture Maps fetch (bbox: {:.4},{:.4},{:.4},{:.4}, themes: {}, cli_version: {})",
        bbox.0,
        bbox.1,
        bbox.2,
        bbox.3,
        theme_names.join(", "),
        cli_version,
    );

    let cache_dir = overture_cache_dir();

    // Flatten all (theme, cli_type) pairs so we can report progress as a
    // fraction of total work.
    let pairs: Vec<(OvertureTheme, &'static str)> = params
        .themes
        .iter()
        .flat_map(|&theme| theme.cli_types().into_iter().map(move |t| (theme, t)))
        .collect();

    let total = pairs.len() as f32;
    let mut accumulated = empty_osm_data();

    for (i, (theme, cli_type)) in pairs.iter().enumerate() {
        let pct = i as f32 / total;
        progress_cb(pct, &format!("Fetching Overture {cli_type}…"));

        let data = fetch_one_theme(*theme, cli_type, bbox, params, &cache_dir, cli_version)?;
        accumulated.merge(data);
    }

    log::info!(
        "Overture Maps fetch complete ({} ways, {} POI nodes, {} address nodes)",
        accumulated.ways.len(),
        accumulated.poi_nodes.len(),
        accumulated.addr_nodes.len(),
    );
    progress_cb(1.0, "Overture data ready");
    Ok(accumulated)
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
    bbox: (f64, f64, f64, f64),
    params: &OvertureParams,
    progress_cb: &mut dyn FnMut(f32, &str),
) -> OsmData {
    if !params.enabled {
        return empty_osm_data();
    }
    if !is_cli_available() {
        log::warn!(
            "Overture Maps CLI not available — skipping Overture data.\n\
             Install with: pip install overturemaps"
        );
        return empty_osm_data();
    }

    let cli_version = cached_cli_version();
    let cache_dir = overture_cache_dir();

    let pairs: Vec<(OvertureTheme, &'static str)> = params
        .themes
        .iter()
        .flat_map(|&theme| theme.cli_types().into_iter().map(move |t| (theme, t)))
        .collect();

    let total = pairs.len() as f32;
    let mut accumulated = empty_osm_data();

    for (i, (theme, cli_type)) in pairs.iter().enumerate() {
        let pct = i as f32 / total;
        progress_cb(pct, &format!("Fetching Overture {cli_type}…"));

        match fetch_one_theme(*theme, cli_type, bbox, params, &cache_dir, cli_version) {
            Ok(data) => accumulated.merge(data),
            Err(e) => log::warn!("Skipping Overture type '{cli_type}': {e}"),
        }
    }

    progress_cb(1.0, "Overture data ready");
    accumulated
}
