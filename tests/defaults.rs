// Unit tests for defaults::parse_interval_secs

use onedrive_mount::defaults::parse_interval_secs;

#[test]
fn parse_seconds() {
    assert_eq!(parse_interval_secs("30s"), Some(30));
}

#[test]
fn parse_minutes() {
    assert_eq!(parse_interval_secs("5m"), Some(300));
}

#[test]
fn parse_hours() {
    assert_eq!(parse_interval_secs("2h"), Some(7200));
}

#[test]
fn parse_whitespace_trimmed() {
    assert_eq!(parse_interval_secs("  15m  "), Some(900));
}

#[test]
fn parse_invalid_returns_none() {
    assert_eq!(parse_interval_secs("abc"), None);
    assert_eq!(parse_interval_secs(""), None);
    assert_eq!(parse_interval_secs("5x"), None);
}

#[test]
fn parse_zero() {
    assert_eq!(parse_interval_secs("0s"), Some(0));
    assert_eq!(parse_interval_secs("0m"), Some(0));
}
