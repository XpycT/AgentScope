//! Time formatting helpers. Pure except `format_local`, which reads the host zone.

use chrono::{DateTime, FixedOffset, Local, TimeZone, Utc};

use crate::domain::Timestamp;

/// Compact age label (`12s`, `9m`, `2h`, `1d`). Future timestamps clamp to `0s`.
pub fn relative_label(then: Timestamp, now: Timestamp) -> String {
    let secs = now.saturating_sub(then).max(0) / 1000;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// Format `ts` at an explicit UTC offset, e.g. `2026-03-08 08:00:00 -04:00`.
/// Out-of-range offsets or timestamps fall back to UTC / the literal `invalid time`.
pub fn format_absolute(ts: Timestamp, offset_secs: i32) -> String {
    let offset = FixedOffset::east_opt(offset_secs)
        .unwrap_or_else(|| FixedOffset::east_opt(0).expect("zero offset"));
    match Utc.timestamp_millis_opt(ts).single() {
        Some(utc) => utc
            .with_timezone(&offset)
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string(),
        None => "invalid time".to_string(),
    }
}

/// Format `ts` in the host's local zone, using the offset in effect at `ts`.
pub fn format_local(ts: Timestamp) -> String {
    match DateTime::<Utc>::from_timestamp_millis(ts) {
        Some(utc) => {
            let local = utc.with_timezone(&Local);
            format_absolute(ts, local.offset().local_minus_utc())
        }
        None => "invalid time".to_string(),
    }
}

/// Short local date for compact views, e.g. `Oct 31, 09:12`.
pub fn format_local_short(ts: Timestamp) -> String {
    match DateTime::<Utc>::from_timestamp_millis(ts) {
        Some(utc) => utc
            .with_timezone(&Local)
            .format("%b %-d, %H:%M")
            .to_string(),
        None => "invalid time".to_string(),
    }
}
