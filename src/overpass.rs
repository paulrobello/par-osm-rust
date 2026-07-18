// src/overpass.rs
//! Overpass API integration: QL query builder and HTTP fetch.

use std::borrow::Cow;

use anyhow::{Result, bail};
use reqwest::header::{CONTENT_TYPE, USER_AGENT};
use url::Url;

use crate::filter::FeatureFilter;
use crate::osm::OsmData;

const DEFAULT_OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";
const OVERPASS_TIMEOUT_SECS: u64 = 60;
const OVERPASS_USER_AGENT: &str = concat!(
    env!("CARGO_PKG_NAME"),
    "/",
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("CARGO_PKG_REPOSITORY"),
    ")"
);

/// Approved Overpass API hostnames. Only HTTPS URLs whose host appears in this
/// list are accepted. All other values are rejected to prevent SSRF attacks.
const ALLOWED_OVERPASS_HOSTS: &[&str] = &[
    "overpass-api.de",
    "overpass.kumi.systems",
    "overpass.openstreetmap.ru",
    "maps.mail.ru",
    "overpass.osm.ch",
];

/// Validate that `url` is a safe Overpass endpoint.
///
/// Rejects any URL that:
/// - does not use HTTPS,
/// - includes userinfo,
/// - specifies a port other than 443 (explicit `:443` or no port both pass), or
/// - whose host is not in `ALLOWED_OVERPASS_HOSTS`.
///
/// Returns `Ok(())` if the URL is acceptable, or an error with a descriptive
/// message otherwise.
pub fn validate_overpass_url(url: &str) -> Result<()> {
    let parsed =
        Url::parse(url).map_err(|err| anyhow::anyhow!("Invalid Overpass URL '{url}': {err}"))?;

    if parsed.scheme() != "https" {
        bail!("Overpass URL must use HTTPS (got: '{url}')");
    }

    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("Overpass URL must not include userinfo");
    }

    // SEC-011: pin the port to 443 on allowlisted HTTPS hosts. Both an
    // explicit `:443` and an omitted port are accepted; any other port is
    // rejected. This tightens the SSRF allowlist so a compromised allowlisted
    // mirror cannot redirect traffic to an unrelated service bound to a
    // non-443 port on the same host.
    if let Some(port) = parsed.port() {
        if port != 443 {
            bail!(
                "Overpass URL must use port 443 (or omit the port) on an approved host; \
                 got port {port}"
            );
        }
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("Overpass URL has no host"))?;

    if !ALLOWED_OVERPASS_HOSTS.contains(&host) {
        bail!(
            "Overpass host '{}' is not in the approved list. \
             Allowed hosts: {}",
            host,
            ALLOWED_OVERPASS_HOSTS.join(", ")
        );
    }

    Ok(())
}

/// Resolve the Overpass API URL.
///
/// Reads the `OVERPASS_URL` environment variable **live on every call** (env
/// reads are cheap), so changes to the env var between fetches take effect
/// immediately — there is no process-wide freeze (the previous `OnceLock`
/// cache surprised callers who set the env var between fetches).
///
/// Priority: `OVERPASS_URL` env var → hardcoded [`DEFAULT_OVERPASS_URL`].
///
/// Returns [`Cow<'static, str>`]: a borrow of the [`DEFAULT_OVERPASS_URL`]
/// static when the env var is unset (no allocation), or an owned `String`
/// wrapped in the `Cow` when it is set. Callers that need a `&str` should bind
/// the result to a local and borrow it (see `sources::merge_source_data`),
/// rather than relying on a `'static` reference.
pub fn default_overpass_url() -> Cow<'static, str> {
    match std::env::var("OVERPASS_URL") {
        Ok(url) => Cow::Owned(url),
        Err(_) => Cow::Borrowed(DEFAULT_OVERPASS_URL),
    }
}

/// Build an Overpass QL query (XML output) for the given bounding box,
/// including only the feature types enabled in `filter`.
///
/// `bbox` is `(south, west, north, east)` in decimal degrees.
pub fn build_overpass_query(bbox: (f64, f64, f64, f64), filter: &FeatureFilter) -> Result<String> {
    let (south, west, north, east) = bbox;
    if south >= north {
        bail!("invalid bbox: south ({south}) must be less than north ({north})");
    }
    if west >= east {
        bail!("invalid bbox: west ({west}) must be less than east ({east})");
    }

    let b = format!("{south},{west},{north},{east}");
    let mut parts: Vec<String> = Vec::new();

    if filter.roads {
        parts.push(format!(r#"way["highway"]({b});"#));
    }
    if filter.buildings {
        parts.push(format!(r#"way["building"]({b});"#));
        // Named addresses on standalone nodes (entrance/door nodes in OSM)
        parts.push(format!(r#"node["addr:housenumber"]({b});"#));
    }
    if filter.water {
        parts.push(format!(r#"way["waterway"]({b});"#));
        parts.push(format!(r#"way["natural"="water"]({b});"#));
    }
    if filter.landuse {
        parts.push(format!(r#"way["landuse"]({b});"#));
        parts.push(format!(r#"way["natural"]({b});"#));
    }
    if filter.railways {
        parts.push(format!(r#"way["railway"="rail"]({b});"#));
    }
    // Point and POI features are always included because they are lightweight
    // and provide visible world detail independent of the larger feature filters.
    for element in ["node", "way"] {
        parts.push(format!(r#"{element}["amenity"]({b});"#));
        parts.push(format!(r#"{element}["shop"]({b});"#));
        parts.push(format!(r#"{element}["tourism"]({b});"#));
        parts.push(format!(r#"{element}["leisure"]({b});"#));
        parts.push(format!(r#"{element}["historic"]({b});"#));
        parts.push(format!(
            r#"{element}["man_made"~"^(tower|water_tower|chimney)$"]({b});"#
        ));
    }
    parts.push(format!(r#"node["natural"="tree"]({b});"#));
    parts.push(format!(r#"node["natural"~"^(peak|rock|spring)$"]({b});"#));

    if parts.is_empty() {
        bail!("all feature types are disabled — nothing to query");
    }

    Ok(format!(
        "[out:xml][timeout:{OVERPASS_TIMEOUT_SECS}];\n({});\nout body;>;out skel qt;",
        parts.join("")
    ))
}

/// Maximum number of bytes from an Overpass error response body that will be
/// surfaced into the `anyhow` error message. Mirrors `STDERR_SNIPPET_LIMIT`
/// in `src/overture.rs` — keeps error messages bounded if a mirror returns a
/// large error body.
const ERROR_BODY_LIMIT: usize = 4096;

/// Truncate an Overpass error response body to [`ERROR_BODY_LIMIT`] bytes,
/// splitting across head and tail at char boundaries. Mirrors the
/// `stderr_suffix` pattern in `src/overture.rs`.
fn truncate_error_body(body: &[u8]) -> String {
    let body = String::from_utf8_lossy(body);
    let body = body.trim();
    if body.is_empty() {
        String::new()
    } else if body.len() <= ERROR_BODY_LIMIT {
        body.to_string()
    } else {
        let head_len = ERROR_BODY_LIMIT / 2;
        let tail_len = ERROR_BODY_LIMIT - head_len;
        let head = str_prefix_at_boundary(body, head_len);
        let tail = str_suffix_at_boundary(body, tail_len);
        let omitted = body.len().saturating_sub(head.len() + tail.len());
        format!("{head}\n...[body truncated, {omitted} bytes omitted]...\n{tail}")
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

/// Lazily-initialized shared `reqwest::blocking::Client` for Overpass requests.
///
/// Reused across calls to enable HTTP connection pooling — repeated fetches
/// to the same allowlisted mirror reuse the underlying TCP/TLS connection
/// instead of paying setup cost on every call (ARC-020).
///
/// Per-module configuration preserved on the pooled client:
///   - `redirect(Policy::none())` — Phase 1 SSRF hardening (SEC-002): never
///     follow redirects so a compromised allowlisted mirror cannot bypass the
///     URL allowlist by 30x-ing to an internal host.
///   - `OVERPASS_TIMEOUT_SECS` (60 s) request timeout.
///
/// Built with `OnceLock::get_or_init` so the first successful build is reused
/// for the process lifetime; a build failure propagates as an `anyhow::Error`
/// via `?` instead of panicking. (`OnceLock::get_or_try_init` is unstable on
/// stable Rust — see issue #109737 — so we check-then-init manually. Two
/// racing callers may each build a client; the loser's is discarded.)
fn shared_client() -> Result<&'static reqwest::blocking::Client> {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c);
    }
    let c = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(OVERPASS_TIMEOUT_SECS))
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build Overpass HTTP client: {e}"))?;
    Ok(CLIENT.get_or_init(|| c))
}

/// Fetch raw OSM XML from the Overpass API for the given bounding box.
///
/// - Validates `overpass_url` against an approved host allowlist (SSRF guard).
/// - Validates `bbox` before making any network request.
/// - Returns a user-readable error for HTTP 429 (server busy).
/// - Uses the pooled blocking `reqwest` client (call from `spawn_blocking`);
///   see [`shared_client`] for why the client is reused across calls.
/// - Follows no redirects: any 3xx is treated as an error so a compromised
///   allowlisted mirror cannot redirect the POST to an internal host (the
///   allowlist is only enforced against the initial URL).
pub fn fetch_osm_xml(
    bbox: (f64, f64, f64, f64),
    filter: &FeatureFilter,
    overpass_url: &str,
) -> Result<String> {
    validate_overpass_url(overpass_url)?;
    let query = build_overpass_query(bbox, filter)?;

    let client = shared_client()?;

    let request = build_overpass_request(client, overpass_url, &query)?;
    let res = client.execute(request)?;

    if res.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        bail!("Overpass is busy — try again in a few minutes");
    }
    if !res.status().is_success() {
        let status = res.status();
        let body = truncate_error_body(&res.bytes().unwrap_or_default());
        bail!("Overpass API error ({status}): {body}");
    }

    Ok(res.text()?)
}

fn build_overpass_request(
    client: &reqwest::blocking::Client,
    overpass_url: &str,
    query: &str,
) -> Result<reqwest::blocking::Request> {
    Ok(client
        .post(overpass_url)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(USER_AGENT, OVERPASS_USER_AGENT)
        .body(format!("data={}", urlencoding::encode(query)))
        .build()?)
}

/// Fetch OSM data from Overpass (or cache) and parse it into `OsmData`.
///
/// - `use_cache = true`:  check cache first; write to cache on miss.
/// - `use_cache = false`: always fetch from Overpass; write result to cache.
pub fn fetch_osm_data(
    bbox: (f64, f64, f64, f64),
    filter: &FeatureFilter,
    use_cache: bool,
    overpass_url: &str,
) -> Result<OsmData> {
    let key = crate::osm_cache::cache_key_for_url(bbox, filter, overpass_url);

    if use_cache {
        if let Some(xml) = crate::osm_cache::read_for_url(&key, overpass_url) {
            log::info!("Cache hit for key {}", &key[..8]);
            return crate::osm::parse_osm_xml_str(&xml);
        }
        // Second-chance: containment lookup
        if let Some(xml) = crate::osm_cache::find_containing_for_url(bbox, filter, overpass_url) {
            log::info!("Cache containment hit — reusing larger cached area");
            return crate::osm::parse_osm_xml_str(&xml);
        }
        log::info!("Cache miss — fetching from Overpass (bbox {bbox:?})");
    } else {
        log::info!("Force-fetching from Overpass (bbox {bbox:?})");
    }

    let xml = fetch_osm_xml(bbox, filter, overpass_url)?;

    if let Err(e) = crate::osm_cache::write_for_url(&key, bbox, filter, &xml, overpass_url) {
        log::warn!("Cache write failed: {e}");
    }

    crate::osm::parse_osm_xml_str(&xml)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::FeatureFilter;

    #[test]
    fn overpass_request_includes_user_agent() {
        let client = reqwest::blocking::Client::builder().build().unwrap();
        let request =
            build_overpass_request(&client, &default_overpass_url(), "node(0,0,1,1);").unwrap();

        let user_agent = request
            .headers()
            .get(USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .unwrap();
        assert!(user_agent.contains("par-osm-rust/"));
        assert_eq!(
            request.headers().get(CONTENT_TYPE).unwrap(),
            "application/x-www-form-urlencoded"
        );
    }

    #[test]
    fn query_includes_all_types_by_default() {
        let filter = FeatureFilter::default();
        let q = build_overpass_query((51.5, -0.13, 51.52, -0.10), &filter).unwrap();
        assert!(q.contains(r#"way["highway"]"#), "missing highway");
        assert!(q.contains(r#"way["building"]"#), "missing building");
        assert!(q.contains(r#"way["waterway"]"#), "missing waterway");
        assert!(
            q.contains(r#"way["natural"="water"]"#),
            "missing natural water"
        );
        assert!(q.contains(r#"way["landuse"]"#), "missing landuse");
        assert!(q.contains(r#"way["railway"="rail"]"#), "missing railway");
        assert!(
            q.contains(r#"node["natural"="tree"]"#),
            "missing tree nodes"
        );
        assert!(
            q.contains(r#"node["natural"~"^(peak|rock|spring)$"]"#),
            "missing nature nodes"
        );
        assert!(
            q.contains(r#"node["man_made"~"^(tower|water_tower|chimney)$"]"#),
            "missing man-made landmark nodes"
        );
        assert!(q.contains(r#"way["amenity"]"#), "missing POI ways");
        assert!(q.contains(r#"way["shop"]"#), "missing shop ways");
    }

    #[test]
    fn query_excludes_disabled_roads() {
        let filter = FeatureFilter {
            roads: false,
            ..FeatureFilter::default()
        };
        let q = build_overpass_query((51.5, -0.13, 51.52, -0.10), &filter).unwrap();
        assert!(!q.contains(r#"way["highway"]"#));
        assert!(q.contains(r#"way["building"]"#)); // others still present
    }

    #[test]
    fn query_excludes_disabled_water() {
        let filter = FeatureFilter {
            water: false,
            ..FeatureFilter::default()
        };
        let q = build_overpass_query((51.5, -0.13, 51.52, -0.10), &filter).unwrap();
        assert!(!q.contains(r#"way["waterway"]"#));
        assert!(!q.contains(r#"way["natural"="water"]"#));
    }

    #[test]
    fn query_contains_bbox_coords() {
        let filter = FeatureFilter::default();
        let q = build_overpass_query((51.5, -0.13, 51.52, -0.10), &filter).unwrap();
        assert!(q.contains("51.5"), "missing south");
        assert!(q.contains("-0.13"), "missing west");
        assert!(q.contains("51.52"), "missing north");
        assert!(q.contains("-0.1"), "missing east");
    }

    #[test]
    fn invalid_bbox_south_gt_north() {
        let filter = FeatureFilter::default();
        let result = build_overpass_query((51.52, -0.13, 51.5, -0.10), &filter);
        assert!(result.is_err(), "should fail when south >= north");
    }

    #[test]
    fn invalid_bbox_west_gt_east() {
        let filter = FeatureFilter::default();
        let result = build_overpass_query((51.5, -0.10, 51.52, -0.13), &filter);
        assert!(result.is_err(), "should fail when west >= east");
    }

    #[test]
    fn all_disabled_still_queries_poi_nodes() {
        // Even when all feature categories are disabled, lightweight point and
        // POI queries are always included.
        let filter = FeatureFilter {
            roads: false,
            buildings: false,
            water: false,
            landuse: false,
            railways: false,
        };
        let q = build_overpass_query((51.5, -0.13, 51.52, -0.10), &filter).unwrap();
        assert!(
            q.contains(r#"node["amenity"]"#),
            "POI node queries should always be present"
        );
        assert!(
            q.contains(r#"way["amenity"]"#),
            "POI way queries should always be present"
        );
        assert!(
            q.contains(r#"node["natural"="tree"]"#),
            "tree node queries should always be present"
        );
        assert!(!q.contains(r#"way["highway"]"#), "roads should be absent");
        assert!(
            !q.contains(r#"way["building"]"#),
            "buildings should be absent"
        );
    }

    // ── validate_overpass_url ──────────────────────────────────────────────

    #[test]
    fn valid_default_overpass_url_is_accepted() {
        assert!(validate_overpass_url("https://overpass-api.de/api/interpreter").is_ok());
    }

    #[test]
    fn valid_mirror_url_is_accepted() {
        assert!(validate_overpass_url("https://overpass.kumi.systems/api/interpreter").is_ok());
    }

    #[test]
    fn http_scheme_is_rejected() {
        let err = validate_overpass_url("http://overpass-api.de/api/interpreter");
        assert!(err.is_err(), "HTTP should be rejected");
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("HTTPS"), "error should mention HTTPS: {msg}");
    }

    #[test]
    fn unknown_host_is_rejected() {
        let err = validate_overpass_url("https://evil.example.com/api/interpreter");
        assert!(err.is_err(), "unknown host should be rejected");
        let msg = err.unwrap_err().to_string();
        assert!(
            msg.contains("approved list"),
            "error should mention approved list: {msg}"
        );
    }

    #[test]
    fn ssrf_metadata_url_is_rejected() {
        assert!(
            validate_overpass_url("https://169.254.169.254/latest/meta-data/").is_err(),
            "AWS metadata URL must be rejected"
        );
    }

    #[test]
    fn internal_ip_http_is_rejected() {
        assert!(
            validate_overpass_url("http://192.168.1.1/overpass").is_err(),
            "RFC-1918 HTTP URL must be rejected"
        );
    }

    #[test]
    fn url_with_port_on_approved_host_is_accepted() {
        assert!(
            validate_overpass_url("https://overpass-api.de:443/api/interpreter").is_ok(),
            "explicit :443 on approved host should be allowed"
        );
    }

    #[test]
    fn url_with_non_443_port_on_approved_host_is_rejected() {
        let err = validate_overpass_url("https://overpass-api.de:8080/api/interpreter");
        assert!(
            err.is_err(),
            "non-443 port on approved host must be rejected (SEC-011)"
        );
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("443"), "error should mention port 443: {msg}");
    }

    #[test]
    fn url_with_non_443_port_on_mirror_is_rejected() {
        assert!(
            validate_overpass_url("https://overpass.kumi.systems:8443/api/interpreter").is_err(),
            "non-443 port on any allowlisted host must be rejected"
        );
    }

    #[test]
    fn url_with_allowed_host_in_userinfo_and_evil_host_is_rejected() {
        assert!(
            validate_overpass_url("https://overpass-api.de:443@evil.example.com/api/interpreter")
                .is_err(),
            "allowed host embedded in userinfo must be rejected"
        );
    }

    #[test]
    fn url_with_userinfo_on_allowed_host_is_rejected() {
        assert!(
            validate_overpass_url("https://user:pass@overpass-api.de/api/interpreter").is_err(),
            "userinfo must be rejected even when host is approved"
        );
    }

    // ── default_overpass_url (ARC-010 / QA-017) ───────────────────────────
    //
    // The env var is read live on every call, so changes between calls take
    // effect immediately (no OnceLock freeze). A module-level Mutex
    // serializes these tests so they don't race on `OVERPASS_URL` with each
    // other; the `EnvGuard` RAII handle restores the prior value (or unsets)
    // on drop so a panic mid-test cannot poison other tests.

    static OVERPASS_URL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII guard that restores `OVERPASS_URL` to its previous value on drop.
    struct EnvGuard {
        previous: Option<String>,
    }
    impl EnvGuard {
        fn set(value: &str) -> Self {
            // SAFETY: tests under this lock are serialized by
            // `OVERPASS_URL_ENV_LOCK`, so there is no concurrent env mutation
            // from this module. (Other test modules in the binary do not
            // touch `OVERPASS_URL`.)
            let previous = std::env::var("OVERPASS_URL").ok();
            // SAFETY: same single-test serialization as above; `set_var` is
            // `unsafe` under Edition 2024 (SEC-007) but mutation is guarded.
            unsafe { std::env::set_var("OVERPASS_URL", value) };
            Self { previous }
        }
        fn unset() -> Self {
            // SAFETY: see `set`.
            let previous = std::env::var("OVERPASS_URL").ok();
            // SAFETY: see `set`.
            unsafe { std::env::remove_var("OVERPASS_URL") };
            Self { previous }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                // SAFETY: see `set`.
                Some(v) => unsafe { std::env::set_var("OVERPASS_URL", v) },
                // SAFETY: see `set`.
                None => unsafe { std::env::remove_var("OVERPASS_URL") },
            }
        }
    }

    #[test]
    fn default_overpass_url_reads_env_live_between_calls() {
        let _guard = OVERPASS_URL_ENV_LOCK.lock().unwrap();

        // Env unset → hardcoded default.
        let _g0 = EnvGuard::unset();
        assert_eq!(default_overpass_url(), DEFAULT_OVERPASS_URL);

        // Set env → reflected immediately on next call (no freeze).
        let _g1 = EnvGuard::set("https://overpass.kumi.systems/api/interpreter");
        assert_eq!(
            default_overpass_url(),
            "https://overpass.kumi.systems/api/interpreter"
        );

        // Change env between calls → next call sees the new value.
        let _g2 = EnvGuard::set("https://overpass.openstreetmap.ru/api/interpreter");
        assert_eq!(
            default_overpass_url(),
            "https://overpass.openstreetmap.ru/api/interpreter"
        );

        // Unset between calls → next call falls back to default.
        drop(_g2);
        drop(_g1);
        let _g3 = EnvGuard::unset();
        assert_eq!(default_overpass_url(), DEFAULT_OVERPASS_URL);
    }
}
