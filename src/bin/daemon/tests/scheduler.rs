// Unit tests for sync_scheduler internals

use crate::sync_scheduler::parse_interval;
use std::time::Duration;

#[test]
fn parse_seconds() {
    assert_eq!(parse_interval("30s"), Some(Duration::from_secs(30)));
}

#[test]
fn parse_minutes() {
    assert_eq!(parse_interval("5m"), Some(Duration::from_secs(300)));
}

#[test]
fn parse_hours() {
    assert_eq!(parse_interval("2h"), Some(Duration::from_secs(7200)));
}

#[test]
fn parse_whitespace_trimmed() {
    assert_eq!(parse_interval("  15m  "), Some(Duration::from_secs(900)));
}

#[test]
fn parse_invalid_returns_none() {
    assert_eq!(parse_interval("invalid"), None);
    assert_eq!(parse_interval(""), None);
    assert_eq!(parse_interval("5x"), None);
    assert_eq!(parse_interval("abc m"), None);
}

#[test]
fn parse_zero() {
    assert_eq!(parse_interval("0s"), None);
    assert_eq!(parse_interval("0m"), None);
}
