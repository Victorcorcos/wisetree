//! Semantic-version comparison helpers.
//!
//! Mirrors `branchlet/src/utils/version-compare.ts`. Prerelease tags are
//! parsed but ignored when comparing, exactly as upstream.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Option<String>,
}

/// Parse `1.2.3` or `v1.2.3-beta.1` into its components. Returns `None` on
/// any malformed input (matches upstream's try/catch contract).
pub fn parse_version(version: &str) -> Option<SemanticVersion> {
    let cleaned = version.strip_prefix('v').unwrap_or(version);
    let mut split = cleaned.splitn(2, '-');
    let version_part = split.next()?;
    let prerelease = split.next().map(|s| s.to_string());

    let mut parts = version_part.split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next()?.parse().ok()?;
    let patch: u64 = parts.next()?.parse().ok()?;

    Some(SemanticVersion {
        major,
        minor,
        patch,
        prerelease,
    })
}

/// True when `version2` is strictly newer than `version1`, ignoring
/// prerelease tags. Returns `false` for any unparseable input.
pub fn is_newer_version(version1: &str, version2: &str) -> bool {
    let (Some(v1), Some(v2)) = (parse_version(version1), parse_version(version2)) else {
        return false;
    };

    if v2.major != v1.major {
        return v2.major > v1.major;
    }
    if v2.minor != v1.minor {
        return v2.minor > v1.minor;
    }
    v2.patch > v1.patch
}

/// True when `version` parses as a semantic version.
pub fn is_valid_version(version: &str) -> bool {
    parse_version(version).is_some()
}
