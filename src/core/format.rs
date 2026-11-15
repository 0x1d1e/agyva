//! Small text helpers shared by the CLI and the TUI: percentage bars,
//! relative times and display-width-aware truncation.

use chrono::{DateTime, Duration, Utc};
use unicode_width::UnicodeWidthChar;

/// `[███░░░]` with `cells` inner cells; `percent` is clamped to 0..=100.
pub fn bar(percent: f64, cells: usize) -> String {
    let p = if percent.is_finite() {
        percent.clamp(0.0, 100.0)
    } else {
        0.0
    };
    let filled = ((p / 100.0) * cells as f64).round() as usize;
    let filled = filled.min(cells);
    format!("[{}{}]", "█".repeat(filled), "░".repeat(cells - filled))
}

/// `72%`, rounded to a whole number.
pub fn percent_text(percent: f64) -> String {
    format!("{:.0}%", percent.clamp(0.0, 100.0))
}

fn span(d: Duration) -> String {
    let secs = d.num_seconds().max(0);
    let (days, hours, mins) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else if mins > 0 {
        format!("{mins}m")
    } else {
        format!("{secs}s")
    }
}

/// `in 2h 5m` for the future, `now` when due.
pub fn until(now: DateTime<Utc>, t: DateTime<Utc>) -> String {
    if t <= now {
        "now".to_owned()
    } else {
        format!("in {}", span(t - now))
    }
}

/// `3m ago`, or `just now`.
pub fn ago(now: DateTime<Utc>, t: DateTime<Utc>) -> String {
    if t >= now - Duration::seconds(5) {
        "just now".to_owned()
    } else {
        format!("{} ago", span(now - t))
    }
}

/// Remove control characters (so hostile text cannot drive the terminal).
pub fn sanitize(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// A name that came from agy (a quota group, a model): control characters
/// removed and the length capped, so hostile or runaway text cannot drive
/// the terminal or push the rest of a line off screen.
pub fn clean_name(s: &str) -> String {
    const MAX: usize = 64;
    let clean = sanitize(s).trim().to_owned();
    if width(&clean) <= MAX {
        clean
    } else {
        truncate(&clean, MAX)
    }
}

/// Display width of `s` after [`sanitize`].
pub fn width(s: &str) -> usize {
    s.chars().filter_map(UnicodeWidthChar::width).sum()
}

/// Fit `s` into `max` terminal columns, ending with `…` when cut. Control
/// characters are dropped; wide characters are never split.
pub fn truncate(s: &str, max: usize) -> String {
    let clean = sanitize(s);
    if width(&clean) <= max {
        return clean;
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in clean.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > max - 1 {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// Pad (or truncate) to exactly `max` columns.
pub fn fit(s: &str, max: usize) -> String {
    let t = truncate(s, max);
    let pad = max.saturating_sub(width(&t));
    format!("{t}{}", " ".repeat(pad))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_and_percent() {
        assert_eq!(bar(50.0, 4), "[██░░]");
        assert_eq!(bar(0.0, 3), "[░░░]");
        assert_eq!(bar(250.0, 3), "[███]");
        assert_eq!(bar(f64::NAN, 3), "[░░░]");
        assert_eq!(percent_text(72.4), "72%");
    }

    #[test]
    fn truncation_is_width_aware_and_strips_controls() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("日本語です", 5), "日本…");
        assert_eq!(width(&truncate("日本語です", 5)), 5);
        assert_eq!(truncate("a\x1b[31mb", 10), "a[31mb");
        assert_eq!(truncate("abc", 0), "");
        assert_eq!(fit("日", 4).len(), "日".len() + 2);
    }

    #[test]
    fn relative_times() {
        let now = Utc::now();
        assert_eq!(until(now, now + Duration::minutes(125)), "in 2h 5m");
        assert_eq!(until(now, now - Duration::minutes(1)), "now");
        assert_eq!(ago(now, now - Duration::minutes(3)), "3m ago");
        assert_eq!(
            until(now, now + Duration::days(2) + Duration::hours(3)),
            "in 2d 3h"
        );
    }
}
