//! CLI / config string parsers for source-selection options.
//!
//! Free functions that convert user-facing strings (CLI flags, YAML config,
//! JSON request bodies) into the [`OvertureTheme`], [`ThemePriority`],
//! [`PoiSourceMode`], and [`OvertureFailureMode`] values consumed by
//! [`crate::sources`]. Donated upstream from `osm-to-bedrock` (audit ARC-011)
//! so both crates share one parsing surface; `osm-to-bedrock` re-exports these
//! via a thin shim rather than maintaining its own copy.

use anyhow::{Result, bail};
use std::collections::HashMap;

// ARC-102: `ThemePriority` is deprecated (never implemented; will be removed
// in 0.3.0). The three priority parsers below remain parseable for
// backwards config-file compatibility and carry matching deprecations.
#[allow(deprecated)]
use crate::overture::{OvertureTheme, ThemePriority};
use crate::sources::{OvertureFailureMode, PoiSourceMode};

/// Parse a `ThemePriority` from a string ("overture", "osm", or "both").
///
/// **Deprecated (ARC-102, never implemented; will be removed in 0.3.0).**
/// The parsed value is accepted but never consulted by any merge path.
#[deprecated(since = "0.2.2", note = "never implemented; will be removed in 0.3.0")]
#[allow(deprecated)]
pub fn parse_theme_priority(s: &str) -> Result<ThemePriority> {
    match s.to_lowercase().as_str() {
        "overture" => Ok(ThemePriority::Overture),
        "osm" => Ok(ThemePriority::Osm),
        "both" => Ok(ThemePriority::Both),
        _ => bail!("unknown priority '{s}' — expected overture, osm, or both"),
    }
}

/// Parse `"building=overture,transportation=osm"` into a priority map.
///
/// **Deprecated (ARC-102, never implemented; will be removed in 0.3.0).**
/// The parsed map is accepted but never consulted by any merge path.
#[deprecated(since = "0.2.2", note = "never implemented; will be removed in 0.3.0")]
#[allow(deprecated)]
pub fn parse_overture_priority(s: &str) -> Result<HashMap<OvertureTheme, ThemePriority>> {
    let mut map = HashMap::new();
    if s.is_empty() {
        return Ok(map);
    }
    for entry in s.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let parts: Vec<&str> = entry.splitn(2, '=').collect();
        if parts.len() != 2 {
            bail!("invalid overture-priority entry '{entry}' — expected 'theme=priority'");
        }
        let theme = OvertureTheme::from_str_loose(parts[0].trim())
            .ok_or_else(|| anyhow::anyhow!("unknown Overture theme '{}'", parts[0].trim()))?;
        let priority = parse_theme_priority(parts[1].trim())?;
        map.insert(theme, priority);
    }
    Ok(map)
}

/// Parse a JSON object style per-theme priority map.
///
/// **Deprecated (ARC-102, never implemented; will be removed in 0.3.0).**
/// The parsed map is accepted but never consulted by any merge path.
#[deprecated(since = "0.2.2", note = "never implemented; will be removed in 0.3.0")]
#[allow(deprecated)]
pub fn parse_overture_priority_map(
    map: &HashMap<String, String>,
) -> Result<HashMap<OvertureTheme, ThemePriority>> {
    map.iter()
        .map(|(theme, priority)| {
            let theme = theme.trim();
            let theme = OvertureTheme::from_str_loose(theme)
                .ok_or_else(|| anyhow::anyhow!("unknown Overture theme '{theme}'"))?;
            let priority = parse_theme_priority(priority.trim())?;
            Ok((theme, priority))
        })
        .collect()
}

/// Parse `"building,transportation,place"` into a `Vec<OvertureTheme>`.
pub fn parse_overture_themes(s: &str) -> Result<Vec<OvertureTheme>> {
    if s.is_empty() {
        return Ok(OvertureTheme::all());
    }
    s.split(',')
        .map(|t| {
            let t = t.trim();
            OvertureTheme::from_str_loose(t)
                .ok_or_else(|| anyhow::anyhow!("unknown Overture theme '{t}'"))
        })
        .collect()
}

/// Parse a JSON array style theme list. An empty list means all themes.
pub fn parse_overture_theme_list(themes: &[String]) -> Result<Vec<OvertureTheme>> {
    if themes.is_empty() {
        return Ok(OvertureTheme::all());
    }
    themes
        .iter()
        .map(|theme| {
            let theme = theme.trim();
            OvertureTheme::from_str_loose(theme)
                .ok_or_else(|| anyhow::anyhow!("unknown Overture theme '{theme}'"))
        })
        .collect()
}

/// Parse a [`PoiSourceMode`] from a string (`"osm-only"`, `"overture-only"`,
/// `"both"`, or `"overture-preferred"` / `"preferred"`). Underscores are
/// normalized to hyphens so `"osm_only"` is accepted as well.
pub fn parse_poi_source_mode(s: &str) -> Result<PoiSourceMode> {
    match s.to_lowercase().replace('_', "-").as_str() {
        "osm" | "osm-only" => Ok(PoiSourceMode::OsmOnly),
        "overture" | "overture-only" => Ok(PoiSourceMode::OvertureOnly),
        "both" => Ok(PoiSourceMode::Both),
        "overture-preferred" | "preferred" => Ok(PoiSourceMode::OverturePreferred),
        _ => bail!(
            "unknown POI source mode '{s}' — expected osm-only, overture-only, both, or overture-preferred"
        ),
    }
}

/// Parse an [`OvertureFailureMode`] from a string (`"fallback"` /
/// `"fallback-to-osm"`, or `"fail"` / `"strict`). Underscores are normalized
/// to hyphens so `"fallback_to_osm"` is accepted as well.
pub fn parse_overture_failure_mode(s: &str) -> Result<OvertureFailureMode> {
    match s.to_lowercase().replace('_', "-").as_str() {
        "fallback" | "fallback-to-osm" => Ok(OvertureFailureMode::FallbackToOsm),
        "fail" | "strict" => Ok(OvertureFailureMode::Fail),
        _ => bail!("unknown Overture failure mode '{s}' — expected fallback-to-osm or fail"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ARC-102: the deprecated parsers remain under test through 0.3.0 so
    // config-file compatibility stays green.
    #[allow(deprecated)]
    #[test]
    fn overture_priority_rejects_invalid_theme() {
        let err = parse_overture_priority("not-a-theme=osm")
            .unwrap_err()
            .to_string();

        assert!(err.contains("unknown Overture theme 'not-a-theme'"));
    }

    #[allow(deprecated)]
    #[test]
    fn overture_priority_rejects_invalid_priority_value() {
        let err = parse_overture_priority("building=preferred")
            .unwrap_err()
            .to_string();

        assert!(err.contains("unknown priority 'preferred'"));
    }

    #[allow(deprecated)]
    #[test]
    fn overture_priority_map_rejects_invalid_priority_value() {
        let map = HashMap::from([("building".to_string(), "preferred".to_string())]);

        let err = parse_overture_priority_map(&map).unwrap_err().to_string();

        assert!(err.contains("unknown priority 'preferred'"));
    }
}
