/// Format a Unix timestamp (seconds since epoch) as YYYY-MM-DD.
pub fn format_unix_date(unix_seconds: i64) -> String {
    let days = unix_seconds / 86400;
    let mut y: i64 = 1970;
    let mut remaining = days;
    loop {
        let yd = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < yd {
            break;
        }
        remaining -= yd;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let mdays = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0;
    for (i, &md) in mdays.iter().enumerate() {
        if remaining < md {
            m = i + 1;
            break;
        }
        remaining -= md;
    }
    let d = remaining + 1;
    format!("{y:04}-{m:02}-{d:02}")
}

/// Get today's date as YYYY-MM-DD.
pub fn today_date() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    format_unix_date(secs)
}
