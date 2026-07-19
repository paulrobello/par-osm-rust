//! Shared cache directory resolution and legacy cache migration.

use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const SHARED_CACHE_NAME: &str = "par-osm-rust";
const LEGACY_CACHE_NAME: &str = "osm-to-bedrock";

/// Summary for migrating all known legacy cache directories.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MigrationReport {
    /// Migration result for the Overpass XML cache.
    pub overpass: CacheMigrationReport,
    /// Migration result for the SRTM tile cache.
    pub srtm: CacheMigrationReport,
    /// Migration result for the Overture GeoJSON cache.
    pub overture: CacheMigrationReport,
}

/// Summary for migrating one legacy cache directory into its shared location.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CacheMigrationReport {
    /// Legacy source directory inspected for entries to migrate.
    pub legacy_dir: PathBuf,
    /// Shared destination directory entries were migrated into.
    pub shared_dir: PathBuf,
    /// Number of legacy entries moved into the shared directory via rename.
    pub moved_files: usize,
    /// Number of legacy entries copied into the shared directory (rename fell
    /// back to copy, e.g. when crossing filesystem boundaries).
    pub copied_files: usize,
    /// Number of legacy entries skipped (non-regular files, symlinks,
    /// directories, or entries whose destination already differed).
    pub skipped_files: usize,
    /// Number of legacy entries removed because an identical entry already
    /// existed in the shared directory.
    pub removed_duplicate_files: usize,
}

/// Return the platform default root for shared par-osm-rust caches.
pub fn shared_cache_root() -> PathBuf {
    platform_cache_root(SHARED_CACHE_NAME)
}

/// Return the platform default root for legacy osm-to-bedrock caches.
pub fn legacy_cache_root() -> PathBuf {
    platform_cache_root(LEGACY_CACHE_NAME)
}

/// Return the Overpass XML cache directory, creating it if possible.
///
/// Pure path resolution: resolves the directory per the priority below and
/// creates it if missing. Performs no legacy migration — call
/// [`migrate_legacy_caches`] once at startup to move legacy
/// `osm-to-bedrock` caches into the shared default location.
///
/// Priority:
/// 1. `PAR_OSM_OVERPASS_CACHE_DIR`
/// 2. `OVERPASS_CACHE_DIR`
/// 3. shared default `overpass` directory
pub fn overpass_cache_dir() -> PathBuf {
    let dir = env_dir("PAR_OSM_OVERPASS_CACHE_DIR")
        .or_else(|| env_dir("OVERPASS_CACHE_DIR"))
        .unwrap_or_else(|| shared_cache_root().join("overpass"));
    ensure_dir(&dir, "Overpass");
    dir
}

/// Return the SRTM tile cache directory, creating it if possible.
///
/// Pure path resolution: resolves the directory per the priority below and
/// creates it if missing. Performs no legacy migration — call
/// [`migrate_legacy_caches`] once at startup to move legacy
/// `osm-to-bedrock` caches into the shared default location.
///
/// Priority:
/// 1. `PAR_OSM_SRTM_CACHE_DIR`
/// 2. `SRTM_CACHE_DIR`
/// 3. shared default `srtm` directory
pub fn srtm_cache_dir() -> PathBuf {
    let dir = env_dir("PAR_OSM_SRTM_CACHE_DIR")
        .or_else(|| env_dir("SRTM_CACHE_DIR"))
        .unwrap_or_else(|| shared_cache_root().join("srtm"));
    ensure_dir(&dir, "SRTM");
    dir
}

/// Return the Overture GeoJSON cache directory, creating it if possible.
///
/// Pure path resolution: resolves the directory per the priority below and
/// creates it if missing. Performs no legacy migration — call
/// [`migrate_legacy_caches`] once at startup to move legacy
/// `osm-to-bedrock` caches into the shared default location.
///
/// Priority:
/// 1. `PAR_OSM_OVERTURE_CACHE_DIR`
/// 2. `OVERTURE_CACHE_DIR`
/// 3. shared default `overture` directory
pub fn overture_cache_dir() -> PathBuf {
    let dir = env_dir("PAR_OSM_OVERTURE_CACHE_DIR")
        .or_else(|| env_dir("OVERTURE_CACHE_DIR"))
        .unwrap_or_else(|| shared_cache_root().join("overture"));
    ensure_dir(&dir, "Overture");
    dir
}

/// Migrate legacy `osm-to-bedrock` Overpass, SRTM, and Overture caches into
/// the shared default location.
///
/// This is the explicit entry point downstream applications call **once at
/// startup**. The [`overpass_cache_dir`], [`srtm_cache_dir`], and
/// [`overture_cache_dir`] getters are pure path resolution and do not migrate
/// anything — if your application needs to pick up pre-existing
/// `~/.cache/osm-to-bedrock/{overpass,srtm,overture}` directories, call this
/// before any cache access.
///
/// # Errors
///
/// Returns `Err` if creating a shared cache directory, reading the legacy
/// directory, or moving/copying an individual legacy entry fails. The
/// function migrates the three subdirectories (overpass, srtm, overture) in
/// sequence; the first subdirectory to fail short-circuits the remaining
/// ones. A missing legacy directory is not an error and yields a zero-count
/// report for that subdirectory.
///
/// # Examples
///
/// Call once at startup, before any cache access. The function inspects and
/// writes to the user's shared cache directories, so the example is `no_run`.
///
/// ```no_run
/// use par_osm_rust::cache::migrate_legacy_caches;
///
/// # fn main() -> anyhow::Result<()> {
/// let report = migrate_legacy_caches()?;
/// println!(
///     "migrated overpass={} srtm={} overture={} entries",
///     report.overpass.moved_files + report.overpass.copied_files,
///     report.srtm.moved_files + report.srtm.copied_files,
///     report.overture.moved_files + report.overture.copied_files,
/// );
/// # Ok(())
/// # }
/// ```
pub fn migrate_legacy_caches() -> Result<MigrationReport> {
    Ok(MigrationReport {
        overpass: migrate_legacy_cache_dir("overpass")?,
        srtm: migrate_legacy_cache_dir("srtm")?,
        overture: migrate_legacy_cache_dir("overture")?,
    })
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn platform_cache_root(app_name: &str) -> PathBuf {
    // ARC-110: platform-correct cache root resolution.
    //
    // Windows: `LOCALAPPDATA` is the conventional per-user application-data
    // location; native Windows installs should land there rather than under
    // `HOME` (which is typically only set via MSYS/Cygwin/Git-Bash shells).
    // Fall back to `HOME/.cache/<app>` if `LOCALAPPDATA` is unset, then to
    // the system temp dir. See QA-020.
    //
    // Unix (incl. macOS): honor `XDG_CACHE_HOME` when set and non-empty
    // (`env_dir` filters empties), then fall back to the conventional
    // `$HOME/.cache/<app>`. macOS deliberately stays on `~/.cache/<app>`
    // rather than `~/Library/Caches/<app>` so existing user caches are not
    // orphaned — switching to the macOS-conventional location would
    // silently hide every entry written by 0.1.x/0.2.x until each user
    // either re-runs `migrate_legacy_caches` or re-fetches. Migration to
    // `~/Library/Caches` is out of scope for this cycle.
    //
    // `cfg!(windows)` is a compile-time gate, so a unix shell that happens
    // to export `LOCALAPPDATA` (a misconfiguration) cannot override the
    // XDG/HOME-based path — verified by `roots_ignore_localappdata_on_unix`.
    if cfg!(windows) {
        if let Some(local) = env_dir("LOCALAPPDATA") {
            return local.join(app_name);
        }
        if let Some(home) = env_dir("HOME") {
            return home.join(".cache").join(app_name);
        }
    } else {
        if let Some(xdg) = env_dir("XDG_CACHE_HOME") {
            return xdg.join(app_name);
        }
        if let Some(home) = env_dir("HOME") {
            return home.join(".cache").join(app_name);
        }
    }
    std::env::temp_dir().join(app_name)
}

fn ensure_dir(dir: &Path, label: &str) {
    if let Err(err) = fs::create_dir_all(dir) {
        log::warn!(
            "Could not create {label} cache dir {}: {err}",
            dir.display()
        );
    }
}

fn migrate_legacy_cache_dir(subdir: &str) -> Result<CacheMigrationReport> {
    let legacy_dir = legacy_cache_root().join(subdir);
    let shared_dir = shared_cache_root().join(subdir);
    fs::create_dir_all(&shared_dir)
        .with_context(|| format!("creating shared cache dir {}", shared_dir.display()))?;

    let mut report = CacheMigrationReport {
        legacy_dir: legacy_dir.clone(),
        shared_dir: shared_dir.clone(),
        ..CacheMigrationReport::default()
    };

    // Use `symlink_metadata` so a symlinked legacy dir is not traversed.
    // `symlink_metadata` describes the path itself (not its target), so
    // `is_dir()` is false for a symlink — defense against local data-exfil
    // / OOM via hostile symlinks (SEC-006).
    if !matches!(fs::symlink_metadata(&legacy_dir), Ok(m) if m.is_dir()) {
        return Ok(report);
    }

    if !is_dir_empty(&shared_dir)? && subdir != "overture" {
        report.skipped_files = legacy_file_count(&legacy_dir)?;
        return Ok(report);
    }
    // QA-109: overture is exempt from the "shared dir non-empty → skip
    // migration" guard that overpass and srtm enforce. The overture cache
    // is keyed by `(bbox, theme, CLI version)` and accumulates entries across
    // releases, so a populated shared dir is the normal steady state, not a
    // sign that migration already ran. Skipping on non-empty would leave
    // legitimate legacy entries stranded. overpass/srtm, by contrast, key on
    // a single in-flight URL/tile per slot, so a non-empty shared dir means
    // migration already happened and re-running would duplicate work. The
    // asymmetry is intentional; see commit d6b224c ("fix: merge legacy
    // overture cache files") for the original decision.

    for entry in fs::read_dir(&legacy_dir)
        .with_context(|| format!("reading legacy cache dir {}", legacy_dir.display()))?
    {
        let entry = entry?;
        let src = entry.path();
        // Skip symlinks (and any non-regular file) without following them.
        // `symlink_metadata` describes the entry itself rather than its
        // target, so `is_file()` is false for a symlink — a hostile
        // symlinked legacy entry (e.g. one pointing at `/dev/zero` or any
        // arbitrary target) is rejected here rather than migrated. See
        // SEC-006.
        match fs::symlink_metadata(&src) {
            Ok(meta) if meta.is_file() => {}
            _ => {
                report.skipped_files += 1;
                continue;
            }
        }
        let dst = shared_dir.join(entry.file_name());
        migrate_file(&src, &dst, &mut report)?;
    }

    Ok(report)
}

fn is_dir_empty(dir: &Path) -> Result<bool> {
    Ok(fs::read_dir(dir)
        .with_context(|| format!("reading shared cache dir {}", dir.display()))?
        .next()
        .is_none())
}

fn legacy_file_count(legacy_dir: &Path) -> Result<usize> {
    let mut count = 0usize;
    for entry in fs::read_dir(legacy_dir)
        .with_context(|| format!("reading legacy cache dir {}", legacy_dir.display()))?
    {
        let path = entry?.path();
        // `symlink_metadata` so symlinked entries are not counted as files
        // (SEC-006): `is_file()` is false for the symlink itself.
        if matches!(fs::symlink_metadata(&path), Ok(m) if m.is_file()) {
            count += 1;
        }
    }
    Ok(count)
}

fn migrate_file(src: &Path, dst: &Path, report: &mut CacheMigrationReport) -> Result<()> {
    if dst.exists() {
        if files_equal(src, dst)? {
            fs::remove_file(src)
                .with_context(|| format!("removing duplicate legacy file {}", src.display()))?;
            report.removed_duplicate_files += 1;
        } else {
            report.skipped_files += 1;
        }
        return Ok(());
    }

    match fs::rename(src, dst) {
        Ok(()) => {
            report.moved_files += 1;
            Ok(())
        }
        Err(rename_err) => {
            fs::copy(src, dst).with_context(|| {
                format!(
                    "copying legacy cache file {} to {} after rename failed: {rename_err}",
                    src.display(),
                    dst.display()
                )
            })?;
            fs::remove_file(src)
                .with_context(|| format!("removing copied legacy file {}", src.display()))?;
            report.copied_files += 1;
            Ok(())
        }
    }
}

/// Maximum number of bytes examined by [`files_equal`] when comparing two
/// cache entries.
///
/// Bounded IO defends against hostile or pathological inputs (e.g. an
/// attacker-controlled symlinked `/dev/zero` target) while streaming with
/// early-exit bounds peak memory to the chunk size. Two entries whose lengths
/// match and whose first `FILES_EQUAL_MAX_BYTES` bytes match are treated as
/// equal for migration dedup.
const FILES_EQUAL_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Compare two cache entries by streaming their bytes with early exit on the
/// first mismatch (SEC-006 / QA-016).
///
/// Returns `Ok(false)` if either path is not a regular file (symlinks are not
/// followed), if the file lengths differ, if the byte streams differ within
/// the first [`FILES_EQUAL_MAX_BYTES`] bytes, or if either stream ends before
/// the cap is reached. Two non-symlinked regular files of equal length whose
/// first `FILES_EQUAL_MAX_BYTES` bytes match are considered equal — sufficient
/// for migration dedup since cache entries are content-addressed by bbox/URL.
fn files_equal(a: &Path, b: &Path) -> Result<bool> {
    // `symlink_metadata` so symlinked arguments are not followed (SEC-006).
    let a_meta =
        fs::symlink_metadata(a).with_context(|| format!("reading metadata for {}", a.display()))?;
    let b_meta =
        fs::symlink_metadata(b).with_context(|| format!("reading metadata for {}", b.display()))?;
    if !a_meta.is_file() || !b_meta.is_file() {
        return Ok(false);
    }
    if a_meta.len() != b_meta.len() {
        return Ok(false);
    }

    // Stream-compare with early exit on first mismatch so two 25 MB SRTM1
    // tiles that differ in the first KiB cost almost nothing (QA-016). Cap
    // each stream at `FILES_EQUAL_MAX_BYTES` as a defense against unbounded
    // inputs (SEC-006).
    let mut reader_a = fs::File::open(a)
        .with_context(|| format!("opening {}", a.display()))?
        .take(FILES_EQUAL_MAX_BYTES);
    let mut reader_b = fs::File::open(b)
        .with_context(|| format!("opening {}", b.display()))?
        .take(FILES_EQUAL_MAX_BYTES);

    let mut buf_a = [0u8; 8192];
    let mut buf_b = [0u8; 8192];
    loop {
        let read_a = read_fill(&mut reader_a, &mut buf_a)
            .with_context(|| format!("reading {}", a.display()))?;
        let read_b = read_fill(&mut reader_b, &mut buf_b)
            .with_context(|| format!("reading {}", b.display()))?;
        if read_a != read_b {
            return Ok(false);
        }
        if read_a == 0 {
            return Ok(true);
        }
        if buf_a[..read_a] != buf_b[..read_a] {
            return Ok(false);
        }
    }
}

/// Read from `reader` into `buf` until `buf` is full or EOF, returning the
/// number of bytes placed. Used by [`files_equal`] for chunked streaming
/// comparison.
fn read_fill<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = reader
            .read(&mut buf[filled..])
            .with_context(|| "filling read buffer")?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::sync::{Mutex, OnceLock};
    use tempfile::TempDir;

    const ENV_KEYS: &[&str] = &[
        "HOME",
        "LOCALAPPDATA",
        "XDG_CACHE_HOME",
        "PAR_OSM_OVERPASS_CACHE_DIR",
        "OVERPASS_CACHE_DIR",
        "PAR_OSM_SRTM_CACHE_DIR",
        "SRTM_CACHE_DIR",
        "PAR_OSM_OVERTURE_CACHE_DIR",
        "OVERTURE_CACHE_DIR",
    ];

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct EnvSnapshot {
        values: HashMap<&'static str, Option<OsString>>,
    }

    impl EnvSnapshot {
        fn capture() -> Self {
            let values = ENV_KEYS
                .iter()
                .map(|&key| (key, std::env::var_os(key)))
                .collect();
            Self { values }
        }

        fn set_path(&self, key: &str, value: &Path) {
            // SAFETY: `set_var` is `unsafe` under Edition 2024 because env
            // mutation is process-wide and not thread-safe. The `env_lock()`
            // Mutex held by every test in this module serializes all such
            // mutations across the crate's tests, and `EnvSnapshot::drop`
            // restores the captured original value on completion so no
            // mutation leaks across tests.
            unsafe {
                std::env::set_var(key, value);
            }
        }

        fn remove(&self, key: &str) {
            // SAFETY: see `set_path` — Mutex-serialized and restored on drop.
            unsafe {
                std::env::remove_var(key);
            }
        }
    }

    impl Drop for EnvSnapshot {
        fn drop(&mut self) {
            for (&key, value) in &self.values {
                // SAFETY: env mutation is `unsafe` under Edition 2024
                // (process-wide non-thread-safe). The `env_lock()` Mutex
                // serializes every test in this module around the snapshot's
                // lifetime; here we restore the captured value (or remove the
                // key entirely if it was unset at capture time) so the process
                // env is back to its original state before the next test takes
                // the lock.
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }

    fn isolate_cache_env(env: &EnvSnapshot) {
        for key in ENV_KEYS {
            env.remove(key);
        }
    }

    #[test]
    fn shared_and_legacy_roots_use_home_when_available() {
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        env.set_path("HOME", tmp.path());

        assert_eq!(
            shared_cache_root(),
            tmp.path().join(".cache").join("par-osm-rust")
        );
        assert_eq!(
            legacy_cache_root(),
            tmp.path().join(".cache").join("osm-to-bedrock")
        );
    }

    #[cfg(windows)]
    #[test]
    fn roots_use_localappdata_when_home_is_unset() {
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        env.set_path("LOCALAPPDATA", tmp.path());

        assert_eq!(shared_cache_root(), tmp.path().join("par-osm-rust"));
        assert_eq!(legacy_cache_root(), tmp.path().join("osm-to-bedrock"));
    }

    #[cfg(windows)]
    #[test]
    fn roots_prefer_localappdata_when_both_set() {
        // QA-020: on Windows both LOCALAPPDATA and HOME are typically set; the
        // native Windows app-data location (LOCALAPPDATA) must win so cache
        // entries land where native Windows applications and uninstallers
        // expect them.
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        let local = tmp.path().join("localappdata");
        let home = tmp.path().join("home");
        fs::create_dir_all(&local).unwrap();
        fs::create_dir_all(&home).unwrap();
        env.set_path("LOCALAPPDATA", &local);
        env.set_path("HOME", &home);

        assert_eq!(shared_cache_root(), local.join("par-osm-rust"));
        assert_eq!(legacy_cache_root(), local.join("osm-to-bedrock"));
    }

    #[cfg(unix)]
    #[test]
    fn roots_use_xdg_cache_home_when_set() {
        // ARC-110: on unix, XDG_CACHE_HOME takes precedence over $HOME/.cache
        // when set and non-empty (env_dir filters empties).
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        let xdg = tmp.path().join("xdg-cache");
        let home = tmp.path().join("home");
        fs::create_dir_all(&xdg).unwrap();
        fs::create_dir_all(&home).unwrap();
        env.set_path("XDG_CACHE_HOME", &xdg);
        env.set_path("HOME", &home);

        assert_eq!(shared_cache_root(), xdg.join("par-osm-rust"));
        assert_eq!(legacy_cache_root(), xdg.join("osm-to-bedrock"));
    }

    #[cfg(unix)]
    #[test]
    fn roots_ignore_localappdata_on_unix() {
        // ARC-110: LOCALAPPDATA is a Windows-only env var; a misconfigured
        // unix shell that exports it must not override the XDG/HOME-based
        // path. `cfg!(windows)` is the compile-time gate that enforces this.
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        let local = tmp.path().join("localappdata");
        let home = tmp.path().join("home");
        fs::create_dir_all(&local).unwrap();
        fs::create_dir_all(&home).unwrap();
        env.set_path("LOCALAPPDATA", &local);
        env.set_path("HOME", &home);

        assert_eq!(
            shared_cache_root(),
            home.join(".cache").join("par-osm-rust")
        );
        assert_eq!(
            legacy_cache_root(),
            home.join(".cache").join("osm-to-bedrock")
        );
    }

    #[cfg(unix)]
    #[test]
    fn roots_empty_xdg_cache_home_falls_back_to_home() {
        // ARC-110: an empty XDG_CACHE_HOME value is filtered out by env_dir,
        // so resolution falls through to $HOME/.cache/<app> as if XDG were
        // unset (matches the XDG spec — an empty value is "unset").
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        env.set_path("HOME", &home);
        // SAFETY: env-mutation serialized by env_lock(); restored on drop.
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", "");
        }

        assert_eq!(
            shared_cache_root(),
            home.join(".cache").join("par-osm-rust")
        );
    }

    #[test]
    fn overpass_cache_prefers_neutral_env_var() {
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        let neutral = tmp.path().join("neutral-overpass");
        let legacy_override = tmp.path().join("legacy-overpass");
        env.set_path("PAR_OSM_OVERPASS_CACHE_DIR", &neutral);
        env.set_path("OVERPASS_CACHE_DIR", &legacy_override);

        let dir = overpass_cache_dir();

        assert_eq!(dir, neutral);
        assert!(dir.exists());
    }

    #[test]
    fn overpass_cache_uses_legacy_env_var_before_default() {
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let legacy_override = tmp.path().join("legacy-overpass");
        env.set_path("HOME", &home);
        env.set_path("OVERPASS_CACHE_DIR", &legacy_override);

        let dir = overpass_cache_dir();

        assert_eq!(dir, legacy_override);
        assert!(dir.exists());
    }

    #[test]
    fn srtm_cache_prefers_neutral_env_var() {
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        let neutral = tmp.path().join("neutral-srtm");
        let legacy_override = tmp.path().join("legacy-srtm");
        env.set_path("PAR_OSM_SRTM_CACHE_DIR", &neutral);
        env.set_path("SRTM_CACHE_DIR", &legacy_override);

        let dir = srtm_cache_dir();

        assert_eq!(dir, neutral);
        assert!(dir.exists());
    }

    #[test]
    fn srtm_cache_uses_legacy_env_var_before_default() {
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let legacy_override = tmp.path().join("legacy-srtm");
        env.set_path("HOME", &home);
        env.set_path("SRTM_CACHE_DIR", &legacy_override);

        let dir = srtm_cache_dir();

        assert_eq!(dir, legacy_override);
        assert!(dir.exists());
    }

    #[test]
    fn overture_cache_prefers_neutral_env_var() {
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        let neutral = tmp.path().join("neutral-overture");
        let legacy_override = tmp.path().join("legacy-overture");
        env.set_path("PAR_OSM_OVERTURE_CACHE_DIR", &neutral);
        env.set_path("OVERTURE_CACHE_DIR", &legacy_override);

        let dir = overture_cache_dir();

        assert_eq!(dir, neutral);
        assert!(dir.exists());
    }

    #[test]
    fn overture_cache_uses_legacy_env_var_before_default() {
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let legacy_override = tmp.path().join("legacy-overture");
        env.set_path("HOME", &home);
        env.set_path("OVERTURE_CACHE_DIR", &legacy_override);

        let dir = overture_cache_dir();

        assert_eq!(dir, legacy_override);
        assert!(dir.exists());
    }

    #[test]
    fn overpass_cache_dir_does_not_migrate_legacy_files() {
        // Getters are now pure path resolution (ARC-005): the legacy file
        // must remain in place. Explicit migration is covered by
        // `migration_moves_legacy_files_into_empty_shared_dir`.
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        env.set_path("HOME", tmp.path());
        let legacy = tmp.path().join(".cache/osm-to-bedrock/overpass");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("abc.xml"), "<osm />").unwrap();

        let dir = overpass_cache_dir();

        assert_eq!(dir, tmp.path().join(".cache/par-osm-rust/overpass"));
        assert!(!dir.join("abc.xml").exists());
        assert!(legacy.join("abc.xml").exists());
    }

    #[test]
    fn overture_cache_dir_does_not_migrate_legacy_files() {
        // Getters are pure path resolution (ARC-005); migration is an
        // explicit step (`migrate_legacy_caches`) tested separately.
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        env.set_path("HOME", tmp.path());
        let legacy = tmp.path().join(".cache/osm-to-bedrock/overture");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("places.geojson"), "{}").unwrap();
        fs::write(legacy.join("places.meta.json"), "{}").unwrap();

        let dir = crate::overture::overture_cache_dir();

        assert_eq!(dir, tmp.path().join(".cache/par-osm-rust/overture"));
        assert!(!dir.join("places.geojson").exists());
        assert!(!dir.join("places.meta.json").exists());
        assert!(legacy.join("places.geojson").exists());
        assert!(legacy.join("places.meta.json").exists());
    }

    #[test]
    fn overture_cache_dir_does_not_merge_into_non_empty_shared_dir() {
        // Bare getter leaves both legacy and shared dirs untouched.
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        env.set_path("HOME", tmp.path());
        let legacy = tmp.path().join(".cache/osm-to-bedrock/overture");
        let shared = tmp.path().join(".cache/par-osm-rust/overture");
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&shared).unwrap();
        fs::write(legacy.join("area-a.geojson"), "legacy-a").unwrap();
        fs::write(shared.join("area-b.geojson"), "shared-b").unwrap();

        let dir = crate::overture::overture_cache_dir();

        assert_eq!(dir, shared);
        assert!(!dir.join("area-a.geojson").exists());
        assert_eq!(
            fs::read_to_string(dir.join("area-b.geojson")).unwrap(),
            "shared-b"
        );
        assert!(legacy.join("area-a.geojson").exists());
    }

    #[test]
    fn overture_explicit_migration_merges_legacy_files_into_non_empty_shared_dir() {
        // ARC-005: relocated from the old "getter merges" test. Overture is
        // the only subdir that merges into a non-empty shared dir (see
        // `migrate_legacy_cache_dir`).
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        env.set_path("HOME", tmp.path());
        let legacy = tmp.path().join(".cache/osm-to-bedrock/overture");
        let shared = tmp.path().join(".cache/par-osm-rust/overture");
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&shared).unwrap();
        fs::write(legacy.join("area-a.geojson"), "legacy-a").unwrap();
        fs::write(shared.join("area-b.geojson"), "shared-b").unwrap();

        let report = migrate_legacy_cache_dir("overture").unwrap();

        assert_eq!(report.moved_files + report.copied_files, 1);
        assert_eq!(
            fs::read_to_string(shared.join("area-a.geojson")).unwrap(),
            "legacy-a"
        );
        assert_eq!(
            fs::read_to_string(shared.join("area-b.geojson")).unwrap(),
            "shared-b"
        );
        assert!(!legacy.join("area-a.geojson").exists());
    }

    #[test]
    fn overture_cache_dir_with_env_override_does_not_migrate() {
        // Getter resolves to the override and performs no migration;
        // explicit migration is the only path that touches the filesystem.
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        env.set_path("HOME", tmp.path());
        let override_dir = tmp.path().join("custom-overture-cache");
        env.set_path("PAR_OSM_OVERTURE_CACHE_DIR", &override_dir);
        let legacy = tmp.path().join(".cache/osm-to-bedrock/overture");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("places.geojson"), "{}").unwrap();

        let dir = crate::overture::overture_cache_dir();

        assert_eq!(dir, override_dir);
        assert!(dir.exists());
        assert!(legacy.join("places.geojson").exists());
        assert!(
            !tmp.path()
                .join(".cache/par-osm-rust/overture/places.geojson")
                .exists()
        );
    }

    #[test]
    fn migrate_legacy_caches_with_env_override_targets_default_not_override() {
        // ARC-005: explicit migration always targets the default shared dir,
        // independent of any env override the caller has set.
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        env.set_path("HOME", tmp.path());
        let override_dir = tmp.path().join("custom-overture-cache");
        fs::create_dir_all(&override_dir).unwrap();
        env.set_path("PAR_OSM_OVERTURE_CACHE_DIR", &override_dir);
        let legacy = tmp.path().join(".cache/osm-to-bedrock/overture");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("places.geojson"), "{}").unwrap();

        let report = migrate_legacy_caches().unwrap();

        assert_eq!(
            report.overture.moved_files + report.overture.copied_files,
            1
        );
        // Migration went into the default shared dir, not the override.
        assert!(
            tmp.path()
                .join(".cache/par-osm-rust/overture/places.geojson")
                .exists()
        );
        assert!(!override_dir.join("places.geojson").exists());
        assert!(!legacy.join("places.geojson").exists());
    }

    #[test]
    fn overture_cache_dir_with_override_matching_default_does_not_migrate() {
        // Setting OVERTURE_CACHE_DIR to the default path resolves the same
        // directory; the getter is still pure and does not migrate.
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        env.set_path("HOME", tmp.path());
        let default_dir = tmp.path().join(".cache/par-osm-rust/overture");
        env.set_path("OVERTURE_CACHE_DIR", &default_dir);
        let legacy = tmp.path().join(".cache/osm-to-bedrock/overture");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("places.geojson"), "{}").unwrap();

        let dir = crate::overture::overture_cache_dir();

        assert_eq!(dir, default_dir);
        assert!(dir.exists());
        assert!(legacy.join("places.geojson").exists());
        assert!(!dir.join("places.geojson").exists());
    }

    #[test]
    fn migration_moves_legacy_files_into_empty_shared_dir() {
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        env.set_path("HOME", tmp.path());
        let legacy = tmp.path().join(".cache/osm-to-bedrock/overpass");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("abc.xml"), "<osm />").unwrap();

        let report = migrate_legacy_cache_dir("overpass").unwrap();

        let shared_file = tmp.path().join(".cache/par-osm-rust/overpass/abc.xml");
        assert_eq!(report.legacy_dir, legacy);
        assert_eq!(
            report.shared_dir,
            tmp.path().join(".cache/par-osm-rust/overpass")
        );
        assert!(shared_file.exists());
        assert!(!report.legacy_dir.join("abc.xml").exists());
        assert_eq!(report.moved_files + report.copied_files, 1);
        assert_eq!(report.skipped_files, 0);
        assert_eq!(report.removed_duplicate_files, 0);
    }

    #[test]
    fn migration_skips_when_shared_dir_already_has_files() {
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        env.set_path("HOME", tmp.path());
        let legacy = tmp.path().join(".cache/osm-to-bedrock/srtm");
        let shared = tmp.path().join(".cache/par-osm-rust/srtm");
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&shared).unwrap();
        fs::write(legacy.join("N38W122.hgt"), "legacy").unwrap();
        fs::write(shared.join("existing.hgt"), "shared").unwrap();

        let report = migrate_legacy_cache_dir("srtm").unwrap();

        assert_eq!(report.skipped_files, 1);
        assert_eq!(report.moved_files, 0);
        assert_eq!(report.copied_files, 0);
        assert!(legacy.join("N38W122.hgt").exists());
    }

    #[test]
    fn migration_removes_identical_legacy_duplicate() {
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        env.set_path("HOME", tmp.path());
        let legacy = tmp.path().join(".cache/osm-to-bedrock/overpass");
        let shared = tmp.path().join(".cache/par-osm-rust/overpass");
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&shared).unwrap();
        let legacy_file = legacy.join("same.xml");
        let shared_file = shared.join("same.xml");
        fs::write(&legacy_file, "same").unwrap();
        fs::write(&shared_file, "same").unwrap();
        let mut report = CacheMigrationReport::default();

        migrate_file(&legacy_file, &shared_file, &mut report).unwrap();

        assert_eq!(report.removed_duplicate_files, 1);
        assert!(!legacy_file.exists());
        assert!(shared_file.exists());
    }

    #[test]
    fn migration_skips_different_existing_destination() {
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        env.set_path("HOME", tmp.path());
        let legacy = tmp.path().join(".cache/osm-to-bedrock/overpass");
        let shared = tmp.path().join(".cache/par-osm-rust/overpass");
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&shared).unwrap();
        let legacy_file = legacy.join("different.xml");
        let shared_file = shared.join("different.xml");
        fs::write(&legacy_file, "legacy").unwrap();
        fs::write(&shared_file, "shared").unwrap();
        let mut report = CacheMigrationReport::default();

        migrate_file(&legacy_file, &shared_file, &mut report).unwrap();

        assert_eq!(report.skipped_files, 1);
        assert!(legacy_file.exists());
        assert_eq!(fs::read_to_string(shared_file).unwrap(), "shared");
    }

    #[test]
    fn migrate_legacy_caches_reports_all_cache_types() {
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        env.set_path("HOME", tmp.path());
        let overpass_legacy = tmp.path().join(".cache/osm-to-bedrock/overpass");
        let srtm_legacy = tmp.path().join(".cache/osm-to-bedrock/srtm");
        let overture_legacy = tmp.path().join(".cache/osm-to-bedrock/overture");
        fs::create_dir_all(&overpass_legacy).unwrap();
        fs::create_dir_all(&srtm_legacy).unwrap();
        fs::create_dir_all(&overture_legacy).unwrap();
        fs::write(overpass_legacy.join("abc.xml"), "<osm />").unwrap();
        fs::write(srtm_legacy.join("N38W122.hgt"), "hgt").unwrap();
        fs::write(overture_legacy.join("places.geojson"), "{}").unwrap();

        let report = migrate_legacy_caches().unwrap();

        assert_eq!(
            report.overpass.moved_files + report.overpass.copied_files,
            1
        );
        assert_eq!(report.srtm.moved_files + report.srtm.copied_files, 1);
        assert_eq!(
            report.overture.moved_files + report.overture.copied_files,
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn migration_skips_symlinked_legacy_entries() {
        // SEC-006: a symlinked legacy entry must be skipped (not followed),
        // defending against local data-exfil / OOM via hostile symlink targets
        // such as `/dev/zero`. Regular entries in the same dir still migrate.
        use std::os::unix::fs::symlink;
        let _guard = env_lock().lock().unwrap();
        let env = EnvSnapshot::capture();
        isolate_cache_env(&env);
        let tmp = TempDir::new().unwrap();
        env.set_path("HOME", tmp.path());
        let legacy = tmp.path().join(".cache/osm-to-bedrock/overpass");
        fs::create_dir_all(&legacy).unwrap();
        let target = tmp.path().join("evil-target");
        fs::write(&target, "evil").unwrap();
        // Hostile symlink: legacy entry that points elsewhere.
        symlink(&target, legacy.join("link.xml")).unwrap();
        // Legitimate regular file in the same legacy dir.
        fs::write(legacy.join("real.xml"), "<osm />").unwrap();

        let report = migrate_legacy_cache_dir("overpass").unwrap();

        assert_eq!(report.skipped_files, 1);
        assert_eq!(report.moved_files + report.copied_files, 1);
        assert!(
            tmp.path()
                .join(".cache/par-osm-rust/overpass/real.xml")
                .exists()
        );
        assert!(
            !tmp.path()
                .join(".cache/par-osm-rust/overpass/link.xml")
                .exists()
        );
        // Symlink itself is left in place; target file is untouched.
        assert!(legacy.join("link.xml").symlink_metadata().is_ok());
        assert_eq!(fs::read_to_string(&target).unwrap(), "evil");
    }
}
