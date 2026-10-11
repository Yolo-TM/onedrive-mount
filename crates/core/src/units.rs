//! Typed durations and byte sizes for the config.
//!
//! The old config stored these as bare `String`s that were never checked and got
//! handed straight to rclone, so a typo in `vfs_cache_mode` or `dir_cache_time`
//! only surfaced as rclone exiting immediately at startup. Parsing into these
//! types at deserialize time means an invalid config is rejected before anything
//! is spawned, and the canonical `Display` form is always something rclone
//! accepts.

use std::fmt;
use std::time::Duration;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

// ---------------------------------------------------------------------------
// Duration
// ---------------------------------------------------------------------------

/// A duration parsed from strings like `90s`, `5m`, `1h30m`, `30d`, `2w`.
///
/// Unlike the `parse_interval_secs` it replaces, this accepts compound values and
/// day/week units, and it requires an explicit unit so `interval = "15"` is an
/// error rather than a silent guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Dur(Duration);

impl Dur {
    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    pub const fn as_secs(self) -> u64 {
        self.0.as_secs()
    }

    pub const fn as_duration(self) -> Duration {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0.is_zero()
    }

    /// Parse a duration. Returns a human-readable reason on failure.
    pub fn parse(s: &str) -> Result<Self, String> {
        let text = s.trim();
        if text.is_empty() {
            return Err("empty duration".into());
        }

        // `0` on its own is unambiguous and means "disabled" for several rclone flags.
        if text == "0" {
            return Ok(Self::from_secs(0));
        }

        let mut total: u64 = 0;
        let mut digits = String::new();
        let mut saw_component = false;
        let mut chars = text.chars().peekable();

        while let Some(c) = chars.next() {
            if c.is_ascii_digit() {
                digits.push(c);
                continue;
            }

            if digits.is_empty() {
                return Err(format!("expected a number before '{c}' in {text:?}"));
            }

            // `ms` is the only two-letter unit we accept, and only so that a value
            // copied out of an rclone man page does not hard-fail.
            let mut unit = c.to_string();
            if c == 'm' && chars.peek() == Some(&'s') {
                chars.next();
                unit.push('s');
            }

            let value: u64 = digits
                .parse()
                .map_err(|_| format!("number too large in {text:?}"))?;
            digits.clear();

            let secs = match unit.as_str() {
                "ms" => {
                    if !value.is_multiple_of(1000) {
                        return Err(format!(
                            "sub-second precision is not supported here ({text:?})"
                        ));
                    }
                    value / 1000
                }
                "s" => value,
                "m" => value.checked_mul(60).ok_or("duration overflows")?,
                "h" => value.checked_mul(3600).ok_or("duration overflows")?,
                "d" => value.checked_mul(86_400).ok_or("duration overflows")?,
                "w" => value.checked_mul(604_800).ok_or("duration overflows")?,
                other => {
                    return Err(format!(
                        "unknown duration unit {other:?} in {text:?} (use s, m, h, d or w)"
                    ));
                }
            };

            total = total.checked_add(secs).ok_or("duration overflows")?;
            saw_component = true;
        }

        if !digits.is_empty() {
            return Err(format!(
                "{text:?} is missing a unit (write {}s, {}m or {}h)",
                digits, digits, digits
            ));
        }
        if !saw_component {
            return Err(format!("{text:?} is not a duration"));
        }

        Ok(Self(Duration::from_secs(total)))
    }
}

impl fmt::Display for Dur {
    /// Canonical form, largest unit first, e.g. `1h30m`. Always parseable by rclone.
    ///
    /// Weeks are accepted on input but never emitted: decomposing into weeks turns
    /// a perfectly ordinary `30d` into `4w2d`, which would silently rewrite the
    /// user's config the first time we saved it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut secs = self.0.as_secs();
        if secs == 0 {
            return f.write_str("0s");
        }

        let mut out = String::new();
        for (unit, size) in [("d", 86_400u64), ("h", 3600), ("m", 60)] {
            let n = secs / size;
            if n > 0 {
                out.push_str(&n.to_string());
                out.push_str(unit);
                secs -= n * size;
            }
        }
        if secs > 0 {
            out.push_str(&secs.to_string());
            out.push('s');
        }
        f.write_str(&out)
    }
}

impl From<Dur> for Duration {
    fn from(d: Dur) -> Self {
        d.0
    }
}

impl Serialize for Dur {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Dur {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Dur;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a duration string such as \"30s\", \"15m\" or \"1h30m\"")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Dur, E> {
                Dur::parse(v).map_err(de::Error::custom)
            }
        }
        d.deserialize_str(V)
    }
}

// ---------------------------------------------------------------------------
// Size
// ---------------------------------------------------------------------------

/// A byte size parsed from strings like `25G`, `8M`, `200M`, `512K`, `4096`.
///
/// Multipliers are 1024-based to match rclone's `SizeSuffix`, and the `i` in
/// `25Gi` is accepted and ignored so both spellings mean the same thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Size(u64);

impl Size {
    pub const fn from_bytes(bytes: u64) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(self) -> u64 {
        self.0
    }

    /// Parse a size. Returns a human-readable reason on failure.
    pub fn parse(s: &str) -> Result<Self, String> {
        let text = s.trim();
        if text.is_empty() {
            return Err("empty size".into());
        }

        let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return Err(format!("{text:?} does not start with a number"));
        }
        let value: u64 = digits
            .parse()
            .map_err(|_| format!("number too large in {text:?}"))?;

        let suffix = text[digits.len()..].trim();
        let suffix = suffix.strip_suffix('i').unwrap_or(suffix); // 25Gi == 25G
        let mult: u64 = match suffix.to_ascii_uppercase().as_str() {
            "" | "B" => 1,
            "K" => 1 << 10,
            "M" => 1 << 20,
            "G" => 1 << 30,
            "T" => 1 << 40,
            "P" => 1 << 50,
            other => {
                return Err(format!(
                    "unknown size unit {other:?} in {text:?} (use K, M, G, T or P)"
                ));
            }
        };

        value
            .checked_mul(mult)
            .map(Self)
            .ok_or_else(|| format!("{text:?} overflows"))
    }
}

impl fmt::Display for Size {
    /// Largest unit that divides exactly, so `26843545600` renders as `25G`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return f.write_str("0");
        }
        for (unit, size) in [
            ("P", 1u64 << 50),
            ("T", 1 << 40),
            ("G", 1 << 30),
            ("M", 1 << 20),
            ("K", 1 << 10),
        ] {
            if self.0.is_multiple_of(size) {
                return write!(f, "{}{}", self.0 / size, unit);
            }
        }
        write!(f, "{}", self.0)
    }
}

impl Serialize for Size {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Size {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Size;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a size string such as \"8M\" or \"25G\"")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Size, E> {
                Size::parse(v).map_err(de::Error::custom)
            }
        }
        d.deserialize_str(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_unit_durations() {
        assert_eq!(Dur::parse("30s").unwrap().as_secs(), 30);
        assert_eq!(Dur::parse("5m").unwrap().as_secs(), 300);
        assert_eq!(Dur::parse("72h").unwrap().as_secs(), 259_200);
        assert_eq!(Dur::parse("30d").unwrap().as_secs(), 2_592_000);
        assert_eq!(Dur::parse("2w").unwrap().as_secs(), 1_209_600);
    }

    /// The two cases the old `defaults::parse_interval_secs` rejected outright.
    #[test]
    fn parses_compound_and_day_durations() {
        assert_eq!(Dur::parse("1h30m").unwrap().as_secs(), 5400);
        assert_eq!(Dur::parse("14d").unwrap().as_secs(), 1_209_600);
        assert_eq!(Dur::parse("1w2d3h4m5s").unwrap().as_secs(), 788_645);
    }

    #[test]
    fn zero_is_allowed_because_rclone_uses_it_to_disable_flags() {
        assert!(Dur::parse("0").unwrap().is_zero());
        assert!(Dur::parse("0s").unwrap().is_zero());
    }

    #[test]
    fn rejects_durations_without_a_unit() {
        let err = Dur::parse("15").unwrap_err();
        assert!(err.contains("missing a unit"), "{err}");
    }

    #[test]
    fn rejects_unknown_units_and_junk() {
        assert!(Dur::parse("5y").is_err());
        assert!(Dur::parse("").is_err());
        assert!(Dur::parse("abc").is_err());
        assert!(Dur::parse("m5").is_err());
        assert!(Dur::parse("5m!").is_err());
    }

    #[test]
    fn rejects_sub_second_precision_but_accepts_whole_second_ms() {
        assert_eq!(Dur::parse("2000ms").unwrap().as_secs(), 2);
        assert!(Dur::parse("500ms").is_err());
    }

    #[test]
    fn duration_overflow_is_an_error_not_a_wrap() {
        assert!(Dur::parse("99999999999999999999s").is_err());
        assert!(Dur::parse("18446744073709551615w").is_err());
    }

    #[test]
    fn duration_display_is_canonical_and_round_trips() {
        for text in ["0s", "30s", "5m", "1h30m", "30d", "9d3h4m5s"] {
            let parsed = Dur::parse(text).unwrap();
            assert_eq!(parsed.to_string(), text, "input {text}");
            assert_eq!(Dur::parse(&parsed.to_string()).unwrap(), parsed);
        }
        // Non-canonical input normalises rather than failing.
        assert_eq!(Dur::parse("90s").unwrap().to_string(), "1m30s");
        assert_eq!(Dur::parse("60m").unwrap().to_string(), "1h");
        // Weeks parse but are rendered in days, so `30d` never becomes `4w2d`.
        assert_eq!(Dur::parse("2w").unwrap().to_string(), "14d");
        assert_eq!(Dur::parse("1w2d3h4m5s").unwrap().to_string(), "9d3h4m5s");
    }

    #[test]
    fn parses_sizes_with_binary_multipliers() {
        assert_eq!(Size::parse("1024").unwrap().as_bytes(), 1024);
        assert_eq!(Size::parse("512K").unwrap().as_bytes(), 524_288);
        assert_eq!(Size::parse("8M").unwrap().as_bytes(), 8 << 20);
        assert_eq!(Size::parse("25G").unwrap().as_bytes(), 25 << 30);
        assert_eq!(Size::parse("25Gi").unwrap().as_bytes(), 25 << 30);
        assert_eq!(Size::parse("200m").unwrap().as_bytes(), 200 << 20);
    }

    #[test]
    fn rejects_bad_sizes() {
        assert!(Size::parse("").is_err());
        assert!(Size::parse("G").is_err());
        assert!(Size::parse("10X").is_err());
        assert!(Size::parse("99999999999999999999G").is_err());
    }

    #[test]
    fn size_display_picks_the_exact_unit_and_round_trips() {
        assert_eq!(Size::from_bytes(25 << 30).to_string(), "25G");
        assert_eq!(Size::from_bytes(8 << 20).to_string(), "8M");
        assert_eq!(Size::from_bytes(0).to_string(), "0");
        // Not a clean multiple of any unit, so it stays in bytes.
        assert_eq!(Size::from_bytes(1025).to_string(), "1025");
        for text in ["25G", "8M", "512K", "1025"] {
            assert_eq!(Size::parse(text).unwrap().to_string(), text);
        }
    }

    #[test]
    fn durations_and_sizes_round_trip_through_toml() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Holder {
            interval: Dur,
            cap: Size,
        }

        let original = Holder {
            interval: Dur::parse("1h30m").unwrap(),
            cap: Size::parse("25G").unwrap(),
        };
        let text = toml::to_string(&original).unwrap();
        assert!(text.contains("interval = \"1h30m\""), "{text}");
        assert!(text.contains("cap = \"25G\""), "{text}");
        assert_eq!(toml::from_str::<Holder>(&text).unwrap(), original);
    }

    #[test]
    fn toml_rejects_an_invalid_duration_with_a_useful_message() {
        #[derive(Debug, Deserialize)]
        struct Holder {
            #[allow(dead_code)]
            interval: Dur,
        }
        let err = toml::from_str::<Holder>("interval = \"15\"").unwrap_err();
        assert!(err.to_string().contains("missing a unit"), "{err}");
    }
}
