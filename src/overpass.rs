// src/overpass.rs
//! Overpass API integration: QL query builder and HTTP fetch.

use std::borrow::Cow;
use std::io::Read;

use anyhow::{Result, bail};
use reqwest::header::{CONTENT_TYPE, USER_AGENT};
use url::Url;

use crate::bbox::BBox;
use crate::filter::FeatureFilter;
use crate::osm::OsmData;
// QA-107: truncation helpers consolidated in `crate::text_truncate`.
use crate::text_truncate::{str_prefix_at_boundary, str_suffix_at_boundary};

const DEFAULT_OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";
const OVERPASS_TIMEOUT_SECS: u64 = 60;

/// Upper bound on a successful Overpass response body. Large-area queries
/// legitimately return hundreds of MB, so the cap is generous (2 GiB); the
/// fetch bails if the body exceeds it. See SEC-109.
const MAX_OVERPASS_RESPONSE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Upper bound on bytes read from an Overpass *error* response. Only
/// [`ERROR_BODY_LIMIT`] bytes are surfaced into the error message after
/// truncation, so reading more than this would be pure waste; 64 KiB is
/// comfortably above the truncation window. See SEC-109.
const MAX_OVERPASS_ERROR_BODY_READ_BYTES: usize = 64 * 1024;
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

/// Validate that `url` is a safe Overpass endpoint, optionally extending the
/// allowlist with consumer-supplied hosts (ARC-107, 0.3.0).
///
/// Rejects any URL that:
/// - does not use HTTPS,
/// - includes userinfo,
/// - specifies a port other than 443 (explicit `:443` or no port both pass), or
/// - whose host is not in `ALLOWED_OVERPASS_HOSTS` AND not an exact match in
///   `extra_hosts`.
///
/// **The `extra_hosts` relaxation is host-only.** HTTPS, no-userinfo, and
/// port-443 are enforced unconditionally — adding a host to `extra_hosts`
/// does NOT relax any other check. The consumer assumes the SSRF exposure
/// that comes with routing traffic to the added host.
///
/// # Errors
///
/// Returns `Err` with a descriptive message naming the failed check if `url`
/// cannot be parsed by `Url::parse`, or if it violates any of the rules above.
pub fn validate_overpass_url_with_hosts(url: &str, extra_hosts: &[String]) -> Result<()> {
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
    // non-443 port on the same host. ARC-107: this check is unconditional —
    // `extra_hosts` only relaxes the host allowlist, not the port guard.
    if let Some(port) = parsed.port()
        && port != 443
    {
        bail!(
            "Overpass URL must use port 443 (or omit the port) on an approved host; \
             got port {port}"
        );
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("Overpass URL has no host"))?;

    let allowed = ALLOWED_OVERPASS_HOSTS.contains(&host) || extra_hosts.iter().any(|h| h == host);
    if !allowed {
        bail!(
            "Overpass host '{}' is not in the approved list. \
             Allowed hosts: {}{}",
            host,
            ALLOWED_OVERPASS_HOSTS.join(", "),
            if extra_hosts.is_empty() {
                String::new()
            } else {
                format!("; extra allowed: {}", extra_hosts.join(", "))
            }
        );
    }

    Ok(())
}

/// Validate that `url` is a safe Overpass endpoint.
///
/// Convenience wrapper around [`validate_overpass_url_with_hosts`] that passes
/// an empty `extra_hosts` slice — i.e. only the hardcoded allowlist
/// (`ALLOWED_OVERPASS_HOSTS`) is consulted. Retained for callers that do not
/// need the ARC-107 extension.
///
/// # Errors
///
/// See [`validate_overpass_url_with_hosts`].
pub fn validate_overpass_url(url: &str) -> Result<()> {
    validate_overpass_url_with_hosts(url, &[])
}

/// Resolve the Overpass API URL.
///
/// Reads the `OVERPASS_URL` environment variable **live on every call** (env
/// reads are cheap), so changes to the env var between fetches take effect
/// immediately — there is no process-wide freeze (the previous `OnceLock`
/// cache surprised callers who set the env var between fetches).
///
/// Priority: `OVERPASS_URL` env var → hardcoded `DEFAULT_OVERPASS_URL`.
///
/// Returns [`Cow<'static, str>`]: a borrow of the `DEFAULT_OVERPASS_URL`
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
/// ARC-106: bbox is the validated [`BBox`] newtype; validation runs once at
/// construction. The legacy `(f64, f64, f64, f64)` SWNE constructor is still
/// reachable via [`BBox::from`].
///
/// # Errors
///
/// Returns `Err` if the bbox fails `crate::bbox::validate_bbox` (SEC-104):
/// non-finite coordinate, latitude outside `[-90, 90]`, longitude outside
/// `[-180, 180]`, or `south >= north` / `west >= east`. All NaN comparisons
/// are false, so the explicit `is_finite()` check inside the validator is
/// what catches NaN (the previous `south >= north` guard could not).
/// Returns `Err` if every feature type is disabled (nothing to query).
pub fn build_overpass_query(bbox: &BBox, filter: &FeatureFilter) -> Result<String> {
    crate::bbox::validate_bbox(bbox.south, bbox.west, bbox.north, bbox.east)?;

    let b = format!("{},{},{},{}", bbox.south, bbox.west, bbox.north, bbox.east);
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
    // ENH-003 resolves the former ARC-105 drift: the `man_made`
    // (tower/water_tower/chimney) and `natural` (peak/rock/spring) standalone
    // nodes fetched above are now classified as POIs by the parsers via
    // `osm::model::POI_TAG_RULES` / `is_poi`, so they land in
    // `OsmData::poi_nodes` with their full tag maps. `natural=tree` is still
    // fetched and routed to `tree_nodes` by the separate parser branch above.
    // The query and classification layers now agree end-to-end.

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
const ERROR_BODY_LIMIT: usize = crate::text_truncate::TRUNCATE_LIMIT;

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

/// Issue the Overpass POST for `bbox`/`filter` and return the streaming
/// [`reqwest::blocking::Response`] **without consuming the body** (ENH-004).
///
/// Single request builder shared by [`fetch_osm_xml`] (which buffers the body
/// into a `String`) and [`fetch_osm_data`] (which streams it straight into the
/// cache). Performs the SSRF URL validation, the `bbox`/filter query build, the
/// pooled-client POST, the no-redirect policy, the 429 check, and the bounded
/// error-body read — then hands back the success response so the caller can
/// pull the body as a [`Read`] (`reqwest::blocking::Response` implements
/// `std::io::Read`).
///
/// # Errors
///
/// Returns `Err` for an invalid/unapproved URL, an all-disabled filter, an HTTP
/// failure, a non-2xx status (with a truncated body in the message), or HTTP
/// 429 (server busy).
fn fetch_osm_response(
    bbox: &BBox,
    filter: &FeatureFilter,
    overpass_url: &str,
    extra_hosts: &[String],
) -> Result<reqwest::blocking::Response> {
    validate_overpass_url_with_hosts(overpass_url, extra_hosts)?;
    let query = build_overpass_query(bbox, filter)?;

    let client = shared_client()?;
    let request = build_overpass_request(client, overpass_url, &query)?;
    let res = client.execute(request)?;

    if res.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        bail!("Overpass is busy — try again in a few minutes");
    }
    if !res.status().is_success() {
        let status = res.status();
        // Bound the error body read: only ERROR_BODY_LIMIT bytes are surfaced
        // after truncation, so reading more than the cap is waste (SEC-109).
        let mut buf = Vec::new();
        res.take(MAX_OVERPASS_ERROR_BODY_READ_BYTES as u64)
            .read_to_end(&mut buf)?;
        let body = truncate_error_body(&buf);
        bail!("Overpass API error ({status}): {body}");
    }
    Ok(res)
}

/// Fetch raw OSM XML from the Overpass API for the given bounding box.
///
/// - Validates `overpass_url` against an approved host allowlist (SSRF guard).
/// - Validates `bbox` before making any network request.
/// - Returns a user-readable error for HTTP 429 (server busy).
/// - Uses the pooled blocking `reqwest` client (call from `spawn_blocking`);
///   see `shared_client` for why the client is reused across calls.
/// - Follows no redirects: any 3xx is treated as an error so a compromised
///   allowlisted mirror cannot redirect the POST to an internal host (the
///   allowlist is only enforced against the initial URL).
///
/// Implemented over the private `fetch_osm_response` plus a capped
/// `read_to_string`, so there is exactly one request builder. [`fetch_osm_data`]
/// streams the body
/// to disk instead of buffering; this function remains the buffer-based public
/// entry point for callers that want the raw XML `String`.
///
/// # Errors
///
/// Returns `Err` if the successful response body exceeds the internal
/// `MAX_OVERPASS_RESPONSE_BYTES` cap (2 GiB, SEC-109). The body is read
/// through a `take(MAX + 1)` adapter so an oversized response is rejected
/// without buffering it fully; the error path is also bounded at the
/// internal `MAX_OVERPASS_ERROR_BODY_READ_BYTES` because only a 4 KiB
/// snippet is surfaced into the error message.
pub fn fetch_osm_xml(
    bbox: &BBox,
    filter: &FeatureFilter,
    overpass_url: &str,
    extra_hosts: &[String],
) -> Result<String> {
    let res = fetch_osm_response(bbox, filter, overpass_url, extra_hosts)?;

    // Bound the success body at MAX + 1 so an oversized response is rejected
    // without buffering it fully (SEC-109). Overpass returns UTF-8 XML, so
    // `read_to_string` matches the prior `.text()` decoding behavior.
    let mut body = String::new();
    res.take(MAX_OVERPASS_RESPONSE_BYTES + 1)
        .read_to_string(&mut body)?;
    if body.len() as u64 > MAX_OVERPASS_RESPONSE_BYTES {
        bail!(
            "Overpass response exceeded {MAX_OVERPASS_RESPONSE_BYTES} byte cap \
             (SEC-109 bound)"
        );
    }
    Ok(body)
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
///
/// **Streaming (ENH-004).** The Overpass response is streamed directly into a
/// cache-directory temp file bounded by the SEC-109 cap, then parsed from that
/// file with the streaming XML parser — the full response body is never held in
/// memory as a `String`, so peak memory on a large fetch is roughly the parsed
/// [`OsmData`] alone (a ~50% cut versus the prior buffer-then-parse path, which
/// kept body + parsed data resident at once). Cache hits likewise parse the
/// cached file by path instead of reading it into a string.
///
/// **Non-fatal cache write.** If the bounded body copy succeeds but committing
/// it into the cache fails (disk full, permissions, …), the fetch still parses
/// from the surviving temp file and only warns — the cache is best-effort. (A
/// failure *during* the bounded copy — over-cap or network I/O — propagates,
/// since under streaming the body lives on disk rather than in memory.)
///
/// # Errors
///
/// Propagates any error from [`validate_overpass_url`], [`fetch_osm_xml`]
/// (HTTP failure, non-2xx status, busy/429), the SEC-109 cap (response exceeds
/// the internal `MAX_OVERPASS_RESPONSE_BYTES` bound), or
/// [`crate::osm::parse_osm_xml_file`] (malformed XML or an unreadable file). A
/// commit-only cache failure is logged and downgraded as described above.
pub fn fetch_osm_data(
    bbox: &BBox,
    filter: &FeatureFilter,
    use_cache: bool,
    overpass_url: &str,
    extra_hosts: &[String],
) -> Result<OsmData> {
    let key = crate::osm_cache::cache_key_for_url(bbox, filter, overpass_url);
    let cache_dir = crate::cache::overpass_cache_dir();

    if use_cache {
        if let Some(path) = crate::osm_cache::data_path_for_url(&cache_dir, &key, overpass_url) {
            log::info!("Cache hit for key {}", &key.as_str()[..8]);
            return crate::osm::parse_osm_xml_file(&path);
        }
        // Second-chance: containment lookup (path-based, parsed by streaming).
        if let Some(path) =
            crate::osm_cache::find_containing_path_for_url(&cache_dir, bbox, filter, overpass_url)
        {
            log::info!("Cache containment hit — reusing larger cached area");
            return crate::osm::parse_osm_xml_file(&path);
        }
        log::info!(
            "Cache miss — fetching from Overpass (bbox {:?})",
            bbox.swne()
        );
    } else {
        log::info!("Force-fetching from Overpass (bbox {:?})", bbox.swne());
    }

    // ENH-004: stream the response straight to the cache directory, bounded by
    // the SEC-109 cap, and parse from the resulting file — never materializing
    // the full body as an in-memory String.
    let res = fetch_osm_response(bbox, filter, overpass_url, extra_hosts)?;
    match crate::osm_cache::stream_write_for_url(
        &cache_dir,
        &key,
        bbox,
        filter,
        res,
        MAX_OVERPASS_RESPONSE_BYTES,
        overpass_url,
    ) {
        Ok(path) => crate::osm::parse_osm_xml_file(&path),
        Err(crate::osm_cache::StreamWriteError::CommitFailed { temp, error }) => {
            // Bounded copy succeeded but the cache commit failed. Preserve the
            // prior non-fatal-cache-write contract: parse from the surviving
            // temp so the fetch still succeeds (`temp` lives through the call).
            log::warn!("Overpass cache commit failed ({error}); parsing without caching");
            crate::osm::parse_osm_xml_file(temp.path())
        }
        Err(crate::osm_cache::StreamWriteError::Hard(error)) => Err(error),
    }
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
        let q = build_overpass_query(&BBox::from((51.5, -0.13, 51.52, -0.10)), &filter).unwrap();
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
        let q = build_overpass_query(&BBox::from((51.5, -0.13, 51.52, -0.10)), &filter).unwrap();
        assert!(!q.contains(r#"way["highway"]"#));
        assert!(q.contains(r#"way["building"]"#)); // others still present
    }

    #[test]
    fn query_excludes_disabled_water() {
        let filter = FeatureFilter {
            water: false,
            ..FeatureFilter::default()
        };
        let q = build_overpass_query(&BBox::from((51.5, -0.13, 51.52, -0.10)), &filter).unwrap();
        assert!(!q.contains(r#"way["waterway"]"#));
        assert!(!q.contains(r#"way["natural"="water"]"#));
    }

    #[test]
    fn query_contains_bbox_coords() {
        let filter = FeatureFilter::default();
        let q = build_overpass_query(&BBox::from((51.5, -0.13, 51.52, -0.10)), &filter).unwrap();
        assert!(q.contains("51.5"), "missing south");
        assert!(q.contains("-0.13"), "missing west");
        assert!(q.contains("51.52"), "missing north");
        assert!(q.contains("-0.1"), "missing east");
    }

    #[test]
    fn invalid_bbox_south_gt_north() {
        let filter = FeatureFilter::default();
        let result =
            build_overpass_query(&BBox::from_unchecked(51.52, -0.13, 51.5, -0.10), &filter);
        assert!(result.is_err(), "should fail when south >= north");
    }

    #[test]
    fn invalid_bbox_west_gt_east() {
        let filter = FeatureFilter::default();
        let result =
            build_overpass_query(&BBox::from_unchecked(51.5, -0.10, 51.52, -0.13), &filter);
        assert!(result.is_err(), "should fail when west >= east");
    }

    // ── SEC-104: NaN / inf / out-of-range bypass ───────────────────────────

    #[test]
    fn invalid_bbox_nan_in_any_position() {
        let filter = FeatureFilter::default();
        // All NaN comparisons are false, so the previous `south >= north`
        // check could not catch NaN; the shared validator's is_finite() does.
        assert!(
            build_overpass_query(
                &BBox::from_unchecked(f64::NAN, -0.13, 51.52, -0.10),
                &filter
            )
            .is_err()
        );
        assert!(
            build_overpass_query(&BBox::from_unchecked(51.5, f64::NAN, 51.52, -0.10), &filter)
                .is_err()
        );
        assert!(
            build_overpass_query(&BBox::from_unchecked(51.5, -0.13, f64::NAN, -0.10), &filter)
                .is_err()
        );
        assert!(
            build_overpass_query(&BBox::from_unchecked(51.5, -0.13, 51.52, f64::NAN), &filter)
                .is_err()
        );
    }

    #[test]
    fn invalid_bbox_infinity_rejected() {
        let filter = FeatureFilter::default();
        assert!(
            build_overpass_query(
                &BBox::from_unchecked(f64::INFINITY, -0.13, 51.52, -0.10),
                &filter
            )
            .is_err()
        );
        assert!(
            build_overpass_query(
                &BBox::from_unchecked(51.5, -0.13, 51.52, f64::NEG_INFINITY),
                &filter
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_bbox_out_of_range_lat_lon_rejected() {
        let filter = FeatureFilter::default();
        // lat 95 is well-ordered but out of range — previously accepted.
        assert!(
            build_overpass_query(&BBox::from_unchecked(95.0, -0.13, 96.0, -0.10), &filter).is_err()
        );
        // lon 200 likewise.
        assert!(
            build_overpass_query(&BBox::from_unchecked(51.5, 199.0, 51.52, 200.0), &filter)
                .is_err()
        );
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
        let q = build_overpass_query(&BBox::from((51.5, -0.13, 51.52, -0.10)), &filter).unwrap();
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

    // ── ARC-107: configurable extra_hosts (host-only relaxation) ────────────

    #[test]
    fn extra_hosts_accepts_exact_match_on_https_443() {
        // Adding a host extends the allowlist; HTTPS + 443 still required.
        let extra = vec!["private-mirror.example.com".to_string()];
        assert!(
            validate_overpass_url_with_hosts(
                "https://private-mirror.example.com/api/interpreter",
                &extra
            )
            .is_ok(),
            "exact host match in extra_hosts must pass when HTTPS+443 hold"
        );
        assert!(
            validate_overpass_url_with_hosts(
                "https://private-mirror.example.com:443/api/interpreter",
                &extra
            )
            .is_ok(),
            "explicit :443 on extra host must pass"
        );
    }

    #[test]
    fn extra_hosts_rejects_substring_match() {
        // Exact match only — a substring of an extra host does NOT pass.
        let extra = vec!["private-mirror.example.com".to_string()];
        assert!(
            validate_overpass_url_with_hosts(
                "https://private-mirror.example.com.evil.example/api/interpreter",
                &extra
            )
            .is_err(),
            "extra_hosts must require exact host equality (no substring)"
        );
        assert!(
            validate_overpass_url_with_hosts(
                "https://evil-private-mirror.example.com/api/interpreter",
                &extra
            )
            .is_err(),
            "extra_hosts must not match on prefix"
        );
    }

    #[test]
    fn extra_hosts_does_not_relax_https() {
        // ARC-107: HTTPS is enforced unconditionally; an extra host on http
        // still fails.
        let extra = vec!["private-mirror.example.com".to_string()];
        assert!(
            validate_overpass_url_with_hosts(
                "http://private-mirror.example.com/api/interpreter",
                &extra
            )
            .is_err(),
            "extra_hosts must NOT relax the HTTPS requirement"
        );
    }

    #[test]
    fn extra_hosts_does_not_relax_non_443_port() {
        // ARC-107: port 443 is enforced unconditionally.
        let extra = vec!["private-mirror.example.com".to_string()];
        assert!(
            validate_overpass_url_with_hosts(
                "https://private-mirror.example.com:8443/api/interpreter",
                &extra
            )
            .is_err(),
            "extra_hosts must NOT relax the port-443 requirement"
        );
    }

    #[test]
    fn extra_hosts_does_not_relax_userinfo() {
        // ARC-107: no-userinfo is enforced unconditionally.
        let extra = vec!["private-mirror.example.com".to_string()];
        assert!(
            validate_overpass_url_with_hosts(
                "https://user:pass@private-mirror.example.com/api/interpreter",
                &extra
            )
            .is_err(),
            "extra_hosts must NOT relax the no-userinfo requirement"
        );
    }

    #[test]
    fn validate_overpass_url_delegates_with_empty_extra_hosts() {
        // The no-args wrapper passes an empty slice; behavior matches the
        // pre-ARC-107 allowlist.
        assert!(validate_overpass_url("https://overpass-api.de/api/interpreter").is_ok());
        assert!(
            validate_overpass_url("https://private-mirror.example.com/api/interpreter").is_err()
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
