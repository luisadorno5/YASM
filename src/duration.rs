//! Parser and formatter for .NET `TimeSpan` strings (`d.hh:mm:ss[.fffffff]` and `hh:mm:ss[.fffffff]`).

use serde::{Deserialize, Deserializer, Serializer};
use std::time::Duration;

/// Parse a .NET `TimeSpan` string such as `"0.00:00:10"`, `"00:01:00"`, or `"1.02:03:04.500"`.
pub fn parse_timespan(input: &str) -> Result<Duration, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("Duration cannot be empty".to_string());
    }
    if s.starts_with('-') {
        return Err(format!("Duration cannot be negative: '{s}'"));
    }

    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return Err(format!(
            "Invalid duration '{s}' (expected format 'd.hh:mm:ss' or 'hh:mm:ss')"
        ));
    }

    // Parse optional days and hours from first segment (`d.hh` or `hh`)
    let (days, hours) = if let Some((d_str, h_str)) = parts[0].split_once('.') {
        let d: u64 = d_str
            .parse()
            .map_err(|_| format!("Invalid days in duration '{s}'"))?;
        let h: u64 = h_str
            .parse()
            .map_err(|_| format!("Invalid hours in duration '{s}'"))?;
        (d, h)
    } else {
        let h: u64 = parts[0]
            .parse()
            .map_err(|_| format!("Invalid hours in duration '{s}'"))?;
        (0, h)
    };

    let minutes: u64 = parts[1]
        .parse()
        .map_err(|_| format!("Invalid minutes in duration '{s}'"))?;

    let (seconds, nanos) = if let Some((sec_str, frac_str)) = parts[2].split_once('.') {
        let sec: u64 = sec_str
            .parse()
            .map_err(|_| format!("Invalid seconds in duration '{s}'"))?;
        if frac_str.is_empty() || frac_str.len() > 9 || !frac_str.chars().all(|c| c.is_ascii_digit())
        {
            return Err(format!("Invalid fractional seconds in duration '{s}'"));
        }
        let padded = format!("{frac_str:0<9}");
        let ns: u32 = padded
            .parse()
            .map_err(|_| format!("Invalid fractional seconds in duration '{s}'"))?;
        (sec, ns)
    } else {
        let sec: u64 = parts[2]
            .parse()
            .map_err(|_| format!("Invalid seconds in duration '{s}'"))?;
        (sec, 0)
    };

    if hours >= 24 || minutes >= 60 || seconds >= 60 {
        return Err(format!(
            "Duration components out of range in '{s}' (hours < 24, minutes < 60, seconds < 60)"
        ));
    }

    let total_secs = days
        .checked_mul(86_400)
        .and_then(|v| v.checked_add(hours * 3_600))
        .and_then(|v| v.checked_add(minutes * 60))
        .and_then(|v| v.checked_add(seconds))
        .ok_or_else(|| format!("Duration overflow in '{s}'"))?;

    Ok(Duration::new(total_secs, nanos))
}

/// Format a [`Duration`] into the company `d.hh:mm:ss` format.
pub fn format_timespan(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let days = total_secs / 86_400;
    let rem = total_secs % 86_400;
    let hours = rem / 3_600;
    let rem = rem % 3_600;
    let minutes = rem / 60;
    let seconds = rem % 60;
    let millis = duration.subsec_millis();
    if millis > 0 {
        format!("{days}.{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
    } else {
        format!("{days}.{hours:02}:{minutes:02}:{seconds:02}")
    }
}

/// Format a [`Duration`] as a compact human-readable uptime string (e.g. `"00:04:12"` or `"2d 03:15:00"`).
pub fn format_uptime(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let days = total_secs / 86_400;
    let rem = total_secs % 86_400;
    let hours = rem / 3_600;
    let rem = rem % 3_600;
    let minutes = rem / 60;
    let seconds = rem % 60;
    if days > 0 {
        format!("{days}d {hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    }
}

pub mod option_timespan_serde {
    use super::*;

    pub fn serialize<S>(value: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(d) => serializer.serialize_some(&format_timespan(*d)),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt: Option<String> = Option::deserialize(deserializer)?;
        match opt {
            Some(s) => parse_timespan(&s)
                .map(Some)
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_timespan_should_parse_standard_company_format() {
        // Arrange / Act / Assert
        assert_eq!(parse_timespan("0.00:00:10").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_timespan("0.00:01:00").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_timespan("0.00:10:00").unwrap(), Duration::from_secs(600));
        assert_eq!(
            parse_timespan("1.02:03:04").unwrap(),
            Duration::from_secs(86_400 + 7_200 + 180 + 4)
        );
        assert_eq!(
            parse_timespan("0.00:00:00.250").unwrap(),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn parse_timespan_should_reject_invalid_strings() {
        // Arrange / Act / Assert
        assert!(parse_timespan("two seconds").is_err());
        assert!(parse_timespan("-0.00:00:10").is_err());
        assert!(parse_timespan("0.25:00:00").is_err());
        assert!(parse_timespan("0.00:60:00").is_err());
        assert!(parse_timespan("0.00:00:60").is_err());
        assert!(parse_timespan("").is_err());
    }
}
