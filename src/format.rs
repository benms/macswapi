//! Text formatting for tables, CSV and the detail view.

pub fn human(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = bytes;
    let mut i = 0;
    while v.abs() >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{}{}", v as i64, UNITS[i])
    } else {
        format!("{:.2}{}", v, UNITS[i])
    }
}

pub fn human_signed(delta: f64) -> String {
    let sign = if delta < 0.0 { "-" } else { "+" };
    format!("{}{}", sign, human(delta.abs()))
}

/// N/A when the kernel's `task_vm_info` revision lacks the field.
pub fn opt_bytes(bytes: Option<u64>) -> String {
    bytes.map_or_else(|| "N/A".to_string(), |b| human(b as f64))
}

/// The kernel reports 0 both for "no jetsam limit" and for a process at or
/// over a soft limit; show it as "-" rather than an alarming 0B.
pub fn headroom_str(remaining: Option<u64>) -> String {
    match remaining {
        None => "N/A".to_string(),
        Some(0) => "-".to_string(),
        Some(b) => human(b as f64),
    }
}

/// Compact duration: 42s, 13m05s, 5h03m, 3d04h.
pub fn duration_str(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        3600..=86_399 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d{:02}h", s / 86_400, s % 86_400 / 3600),
    }
}

/// Footprint history as block characters, scaled to its own range. A spread
/// under 5% of the peak (or 1M) draws low, so allocator noise doesn't pass
/// for a trend. Blank below two points.
pub fn sparkline(points: &[u64]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let (Some(&lo), Some(&hi)) = (points.iter().min(), points.iter().max()) else {
        return String::new();
    };
    if points.len() < 2 {
        return String::new();
    }
    let scale = (hi - lo).max(hi / 20).max(1 << 20) as f64;
    points
        .iter()
        .map(|&p| BARS[((((p - lo) as f64 / scale) * 7.0).round() as usize).min(7)])
        .collect()
}

/// Seconds since the epoch as ISO 8601 UTC: 2026-10-22T08:15:02Z.
pub fn iso8601_utc(secs: u64) -> String {
    // Days to civil date: Howard Hinnant's algorithm, for days >= 0.
    let days = secs / 86_400;
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    let t = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        t / 3600,
        t % 3600 / 60,
        t % 60
    )
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        String::new()
    } else if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    }
}

/// Left-aligned identity columns: PID NAME, or PID PPID NAME PARENT.
pub fn ident_cols(pid: &str, name: &str, parent: Option<(&str, &str)>) -> String {
    match parent {
        Some((ppid, pname)) => format!(
            "{:<7} {:<7} {:<24} {:<20}",
            pid,
            ppid,
            truncate(name, 24),
            truncate(pname, 20)
        ),
        None => format!("{:<7} {:<24}", pid, truncate(name, 24)),
    }
}

/// Right-aligned metric columns, 12 wide each.
pub fn metric_cols<S: AsRef<str>>(cells: &[S]) -> String {
    cells
        .iter()
        .map(|c| format!(" {:>12}", c.as_ref()))
        .collect()
}

pub fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Whole number for CSV, without a "-0".
pub fn csv_num(v: f64) -> String {
    let r = v.round();
    if r == 0.0 {
        "0".to_string()
    } else {
        format!("{r:.0}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_human() {
        assert_eq!(human(0.0), "0B");
        assert_eq!(human(1024.0), "1.00K");
        assert_eq!(human(1048576.0), "1.00M");
        assert_eq!(human(1073741824.0), "1.00G");
        assert_eq!(human(8_978_432.0), "8.56M");
    }

    #[test]
    fn test_human_signed() {
        assert_eq!(human_signed(0.0), "+0B");
        assert_eq!(human_signed(1024.0), "+1.00K");
        assert_eq!(human_signed(-1048576.0), "-1.00M");
    }

    #[test]
    fn test_sparkline() {
        assert_eq!(sparkline(&[]), "");
        assert_eq!(sparkline(&[5 << 20]), "");
        assert_eq!(sparkline(&[100 << 20; 4]), "▁▁▁▁"); // flat
        assert_eq!(sparkline(&[0, 1 << 20, 2 << 20, 4 << 20]), "▁▃▅█");
        // 100K of noise on 500M stays near the floor
        let noise = [500 << 20, (500 << 20) + 100_000, 500 << 20];
        assert_eq!(sparkline(&noise), "▁▁▁");
    }

    #[test]
    fn test_iso8601_utc() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(951_782_400), "2000-02-29T00:00:00Z"); // leap day
        assert_eq!(iso8601_utc(1_792_646_102), "2026-10-22T05:15:02Z");
    }

    #[test]
    fn test_truncate() {
        assert_eq!(truncate("abcdef", 0), "");
        assert_eq!(truncate("abcdef", 1), "…");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
    }

    #[test]
    fn test_duration_str() {
        assert_eq!(duration_str(-1.0), "0s");
        assert_eq!(duration_str(42.9), "42s");
        assert_eq!(duration_str(785.0), "13m05s");
        assert_eq!(duration_str(18_180.0), "5h03m");
        assert_eq!(duration_str(273_600.0), "3d04h");
    }

    #[test]
    fn test_csv_field_and_num() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_num(-0.4), "0");
        assert_eq!(csv_num(1.6), "2");
    }

    #[test]
    fn test_headroom_str() {
        assert_eq!(headroom_str(None), "N/A");
        assert_eq!(headroom_str(Some(0)), "-"); // no limit (or at/over a soft one)
        assert_eq!(headroom_str(Some(1024)), "1.00K");
    }

    #[test]
    fn test_ident_and_metric_cols() {
        assert_eq!(
            ident_cols("1", "launchd", None),
            "1       launchd                 "
        );
        assert_eq!(
            ident_cols("9", "x", Some(("1", "launchd"))),
            "9       1       x                        launchd             "
        );
        assert_eq!(metric_cols(&["1.00K", "N/A"]), "        1.00K          N/A");
    }
}
