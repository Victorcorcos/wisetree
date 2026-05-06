use wisetree::utils::version::{is_newer_version, is_valid_version, parse_version};

#[test]
fn parses_basic_version() {
    let v = parse_version("1.2.3").expect("valid");
    assert_eq!(v.major, 1);
    assert_eq!(v.minor, 2);
    assert_eq!(v.patch, 3);
    assert!(v.prerelease.is_none());
}

#[test]
fn parses_v_prefix() {
    let v = parse_version("v1.2.3").expect("valid");
    assert_eq!(v.major, 1);
    assert_eq!(v.minor, 2);
    assert_eq!(v.patch, 3);
}

#[test]
fn parses_prerelease() {
    let v = parse_version("1.2.3-beta.1").expect("valid");
    assert_eq!(v.prerelease.as_deref(), Some("beta.1"));
}

#[test]
fn rejects_malformed() {
    assert!(parse_version("1.2").is_none());
    assert!(parse_version("not-a-version").is_none());
    assert!(parse_version("").is_none());
    assert!(parse_version("1.2.x").is_none());
}

#[test]
fn newer_minor() {
    assert!(is_newer_version("0.2.0", "0.3.0"));
}

#[test]
fn newer_major() {
    assert!(is_newer_version("0.2.0", "1.0.0"));
}

#[test]
fn equal_is_not_newer() {
    assert!(!is_newer_version("0.2.0", "0.2.0"));
}

#[test]
fn older_returns_false() {
    assert!(!is_newer_version("0.3.0", "0.2.0"));
}

#[test]
fn prerelease_ignored_in_comparison() {
    assert!(!is_newer_version("1.2.3", "1.2.3-rc.1"));
    assert!(!is_newer_version("1.2.3-rc.1", "1.2.3"));
}

#[test]
fn invalid_input_returns_false() {
    assert!(!is_newer_version("garbage", "1.2.3"));
    assert!(!is_newer_version("1.2.3", "garbage"));
}

#[test]
fn valid_version_predicate() {
    assert!(is_valid_version("1.2.3"));
    assert!(is_valid_version("v1.2.3"));
    assert!(is_valid_version("1.2.3-rc.1"));
    assert!(!is_valid_version("1.2"));
    assert!(!is_valid_version(""));
}
