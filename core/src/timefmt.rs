use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn now_unix() -> u64 {
    unix(SystemTime::now())
}

pub fn from_unix(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

/// Whole days since `t` (0 for future times).
pub fn age_days(t: SystemTime) -> u64 {
    SystemTime::now().duration_since(t).map(|d| d.as_secs() / 86400).unwrap_or(0)
}

/// "just now", "5 min ago", "3 h ago", "12 days ago", "4 mo ago", "2 y ago".
pub fn relative(t: Option<SystemTime>) -> String {
    let Some(t) = t else { return "—".into() };
    let s = SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86399 => format!("{} h ago", s / 3600),
        86400..=172799 => "yesterday".into(),
        _ if s < 86400 * 60 => format!("{} days ago", s / 86400),
        _ if s < 86400 * 730 => format!("{} mo ago", s / (86400 * 30)),
        _ => format!("{} y ago", s / (86400 * 365)),
    }
}

/// Civil date (UTC) from days since the Unix epoch (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub fn date(t: Option<SystemTime>) -> String {
    let Some(t) = t else { return "—".into() };
    let (y, m, d) = civil((unix(t) / 86400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn now_iso() -> String {
    let s = now_unix();
    let (y, m, d) = civil((s / 86400) as i64);
    let r = s % 86400;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", r / 3600, (r / 60) % 60, r % 60)
}
