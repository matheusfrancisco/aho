//! When a schedule fires: `at` (one RFC 3339 instant), `every` (a fixed interval from the moment
//! it was created), `cron` (5 fields, evaluated in UTC). All arithmetic is UTC civil time in the
//! proleptic Gregorian calendar, so there are no daylight-saving gaps or repeated hours: a daily
//! `30 2 * * *` is exactly 24 h apart on every date.

use std::fmt;

use aho_types::Timestamp;
use serde::{Deserialize, Serialize};

pub const SECOND: u64 = 1_000;
pub const MINUTE: u64 = 60 * SECOND;
pub const HOUR: u64 = 60 * MINUTE;
pub const DAY: u64 = 24 * HOUR;

/// The Gregorian calendar (weekdays included) repeats every 400 years; a cron expression that
/// matches nothing in that span matches nothing ever.
const HORIZON_MINUTES: i64 = 400 * 366 * 1440;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecError(pub String);

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SpecError {}

fn err<T>(s: impl Into<String>) -> Result<T, SpecError> {
    Err(SpecError(s.into()))
}

// ───────────────────────────────── civil time ─────────────────────────────────

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(y) => 29,
        _ => 28,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `(year, month 1–12, day 1–31)` for days since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// `0 = Sunday … 6 = Saturday`. 1970-01-01 was a Thursday.
fn weekday(days: i64) -> u32 {
    (days + 4).rem_euclid(7) as u32
}

/// `2026-09-26T09:30:00Z`, with `.mmm` only when the instant is not on a whole second.
pub fn format_utc(t: Timestamp) -> String {
    let ms = t.0;
    let days = (ms / DAY) as i64;
    let rem = ms % DAY;
    let (y, mo, d) = civil_from_days(days);
    let (h, mi, s, frac) = (rem / HOUR, rem % HOUR / MINUTE, rem % MINUTE / SECOND, rem % SECOND);
    if frac == 0 {
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
    } else {
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{frac:03}Z")
    }
}

fn digits(b: &[u8], at: usize, n: usize) -> Option<u32> {
    let s = b.get(at..at + n)?;
    s.iter().try_fold(0u32, |acc, c| c.is_ascii_digit().then(|| acc * 10 + u32::from(c - b'0')))
}

/// RFC 3339 `date-time`: `YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)`; `t`/`z` and a space separator are
/// accepted (RFC 3339 §5.6 note). Fractions beyond milliseconds are truncated.
pub fn parse_rfc3339(s: &str) -> Result<Timestamp, SpecError> {
    let bad = || SpecError(format!("`{s}` is not an RFC 3339 time; write it like 2026-09-27T09:00:00Z or 2026-09-27T11:00:00+02:00"));
    let b = s.trim().as_bytes();
    let field = |at, n| digits(b, at, n).ok_or_else(bad);
    let (y, mo, d) = (field(0, 4)?, field(5, 2)?, field(8, 2)?);
    let (h, mi, sec) = (field(11, 2)?, field(14, 2)?, field(17, 2)?);
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') || b.get(13) != Some(&b':') || b.get(16) != Some(&b':') {
        return Err(bad());
    }
    if !matches!(b.get(10), Some(b'T' | b't' | b' ')) {
        return Err(bad());
    }
    let mut i = 19;
    let mut frac_ms = 0u64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            if i - start < 3 {
                frac_ms = frac_ms * 10 + u64::from(b[i] - b'0');
            }
            i += 1;
        }
        if i == start {
            return Err(bad());
        }
        for _ in (i - start)..3 {
            frac_ms *= 10;
        }
    }
    let offset_min: i64 = match b.get(i) {
        Some(b'Z' | b'z') if i + 1 == b.len() => 0,
        Some(sign @ (b'+' | b'-')) if i + 6 == b.len() && b.get(i + 3) == Some(&b':') => {
            let (oh, om) = (field(i + 1, 2)?, field(i + 4, 2)?);
            if oh > 23 || om > 59 {
                return Err(bad());
            }
            let m = i64::from(oh * 60 + om);
            if *sign == b'-' { -m } else { m }
        }
        _ => return Err(bad()),
    };
    if !(1..=12).contains(&mo) || d == 0 || d > days_in_month(i64::from(y), mo) {
        return err(format!("`{s}` names a day that does not exist"));
    }
    if h > 23 || mi > 59 || sec > 59 {
        return err(format!("`{s}` has an out-of-range time of day (hours 00–23, minutes and seconds 00–59)"));
    }
    let days = days_from_civil(i64::from(y), mo, d);
    let secs = days * 86_400 + i64::from(h * 3600 + mi * 60 + sec) - offset_min * 60;
    if secs < 0 {
        return err(format!("`{s}` is before 1970"));
    }
    Ok(Timestamp(secs as u64 * SECOND + frac_ms))
}

/// `30m`, `1h30m`, `1h 30m`, `90s`, `2d`, `1w`; long unit names (`minutes`, `hours`, …) work too.
pub fn parse_duration(s: &str) -> Result<u64, SpecError> {
    let bad = |why: &str| SpecError(format!("`{s}` is not an interval ({why}); write it like 30m, 2h, 1h30m or 1d"));
    let mut total: u64 = 0;
    let mut rest = s.trim();
    if rest.is_empty() {
        return Err(bad("it is empty"));
    }
    while !rest.is_empty() {
        let n_end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        if n_end == 0 {
            return Err(bad("each part needs a number first"));
        }
        let n: u64 = rest[..n_end].parse().map_err(|_| bad("the number is too large"))?;
        let tail = rest[n_end..].trim_start();
        let u_end = tail.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(tail.len());
        let unit = match tail[..u_end].to_ascii_lowercase().as_str() {
            "s" | "sec" | "secs" | "second" | "seconds" => SECOND,
            "m" | "min" | "mins" | "minute" | "minutes" => MINUTE,
            "h" | "hr" | "hrs" | "hour" | "hours" => HOUR,
            "d" | "day" | "days" => DAY,
            "w" | "wk" | "week" | "weeks" => 7 * DAY,
            "" => return Err(bad("a number has no unit")),
            _ => return Err(bad("units are s, m, h, d, w")),
        };
        total = n.checked_mul(unit).and_then(|v| total.checked_add(v)).ok_or_else(|| bad("it is too long"))?;
        rest = tail[u_end..].trim_start();
    }
    if total == 0 {
        return Err(bad("it is zero"));
    }
    Ok(total)
}

/// `1d2h`, `30m`, `1m30s`, `250ms`.
pub fn format_duration(ms: u64) -> String {
    if !ms.is_multiple_of(SECOND) {
        return format!("{ms}ms");
    }
    let mut out = String::new();
    let mut rest = ms;
    for (unit, name) in [(DAY, "d"), (HOUR, "h"), (MINUTE, "m"), (SECOND, "s")] {
        if rest >= unit {
            out.push_str(&format!("{}{name}", rest / unit));
            rest %= unit;
        }
    }
    if out.is_empty() { "0s".into() } else { out }
}

// ───────────────────────────────── cron ─────────────────────────────────

/// A parsed 5-field expression: `minute hour day-of-month month day-of-week`, UTC.
///
/// Items are `*`, `n`, `a-b`, lists `a,b,c`, and steps `*/n`, `a-b/n`, `a/n`; months and weekdays
/// also by name (`JAN`–`DEC`, `SUN`–`SAT`); weekday `7` is Sunday; `@hourly @daily @midnight
/// @weekly @monthly @yearly @annually` expand to their five fields. Day matching is Vixie cron's:
/// when both day fields are restricted (neither starts with `*`) a day matches if **either**
/// does; otherwise both must.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    dom: u32,
    months: u16,
    dow: u8,
    dom_star: bool,
    dow_star: bool,
}

const MONTHS: &[&str] = &["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const DAYS: &[&str] = &["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

struct FieldDef {
    name: &'static str,
    min: u32,
    max: u32,
    /// Names for `min, min+1, …`.
    names: &'static [&'static str],
}

fn value(item: &str, f: &FieldDef, expr: &str) -> Result<u32, SpecError> {
    if let Some(i) = f.names.iter().position(|n| n.eq_ignore_ascii_case(item)) {
        return Ok(f.min + i as u32);
    }
    let v: u32 = item.parse().map_err(|_| SpecError(format!("cron `{expr}`: `{item}` is not a valid {}", f.name)))?;
    if v < f.min || v > f.max {
        return err(format!("cron `{expr}`: {} must be {}–{}, got {v}", f.name, f.min, f.max));
    }
    Ok(v)
}

/// Bits `min..=max` of one field.
fn field(text: &str, f: &FieldDef, expr: &str) -> Result<u64, SpecError> {
    let mut bits = 0u64;
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((r, s)) => {
                let s: u32 = s
                    .parse()
                    .ok()
                    .filter(|s| *s > 0)
                    .ok_or_else(|| SpecError(format!("cron `{expr}`: the step in `{item}` must be a positive number")))?;
                (r, Some(s))
            }
            None => (item, None),
        };
        let (lo, hi) = if range == "*" {
            (f.min, f.max)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a, f, expr)?, value(b, f, expr)?)
        } else {
            let a = value(range, f, expr)?;
            (a, if step.is_some() { f.max } else { a })
        };
        if lo > hi {
            return err(format!("cron `{expr}`: the range `{range}` runs backwards; write the low end first"));
        }
        let step = step.unwrap_or(1);
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok(bits)
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Cron, SpecError> {
        let src = expr.trim();
        let expanded = match src.to_ascii_lowercase().as_str() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            s if s.starts_with('@') => {
                return err(format!("cron `{src}`: unknown shortcut; use @hourly, @daily, @weekly, @monthly or @yearly"));
            }
            _ => src,
        };
        let f: Vec<&str> = expanded.split_whitespace().collect();
        if f.len() != 5 {
            return err(format!(
                "cron `{src}` has {} fields; it needs exactly 5: minute hour day-of-month month day-of-week (UTC)",
                f.len()
            ));
        }
        let def = |name, min, max, names| FieldDef { name, min, max, names };
        let minutes = field(f[0], &def("minute", 0, 59, &[]), src)?;
        let hours = field(f[1], &def("hour", 0, 23, &[]), src)? as u32;
        let dom = field(f[2], &def("day of month", 1, 31, &[]), src)? as u32;
        let months = field(f[3], &def("month", 1, 12, MONTHS), src)? as u16;
        let dow_bits = field(f[4], &def("day of week", 0, 7, DAYS), src)?;
        let dow = ((dow_bits | (dow_bits >> 7)) & 0x7f) as u8;
        Ok(Cron { minutes, hours, dom, months, dow, dom_star: f[2].starts_with('*'), dow_star: f[4].starts_with('*') })
    }

    fn day_matches(&self, dom: u32, dow: u32) -> bool {
        let (a, b) = (self.dom & (1 << dom) != 0, self.dow & (1 << dow) != 0);
        if self.dom_star || self.dow_star { a && b } else { a || b }
    }

    /// The first matching minute strictly after `t`; `None` if it never matches (e.g. `0 0 30 2 *`).
    pub fn next_after(&self, t: Timestamp) -> Option<Timestamp> {
        let mut m = (t.0 / MINUTE) as i64 + 1;
        let horizon = m + HORIZON_MINUTES;
        while m <= horizon {
            let days = m.div_euclid(1440);
            let (y, mo, d) = civil_from_days(days);
            if self.months & (1 << mo) == 0 {
                let (ny, nm) = if mo == 12 { (y + 1, 1) } else { (y, mo + 1) };
                m = days_from_civil(ny, nm, 1) * 1440;
                continue;
            }
            if !self.day_matches(d, weekday(days)) {
                m = (days + 1) * 1440;
                continue;
            }
            let of_day = m.rem_euclid(1440);
            let (h, mi) = (of_day / 60, of_day % 60);
            if self.hours & (1 << h) == 0 {
                m = days * 1440 + (h + 1) * 60;
                continue;
            }
            if self.minutes & (1 << mi) == 0 {
                m += 1;
                continue;
            }
            return Some(Timestamp(m as u64 * MINUTE));
        }
        None
    }
}

// ───────────────────────────────── spec ─────────────────────────────────

/// How a schedule fires. Serialized in `schedule/Created` bodies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Spec {
    /// Once, at `at`.
    At { at: Timestamp },
    /// At `anchor + k·every_ms`, k ≥ 1 (the anchor is the creation time).
    Every { every_ms: u64, anchor: Timestamp },
    /// Every minute the 5-field expression matches, UTC.
    Cron { expr: String },
}

impl Spec {
    /// The first occurrence strictly after `t`.
    pub fn next_after(&self, t: Timestamp) -> Result<Option<Timestamp>, SpecError> {
        Ok(match self {
            Spec::At { at } => (*at > t).then_some(*at),
            Spec::Every { every_ms, anchor } => {
                if *every_ms == 0 {
                    return err("an interval of zero never advances");
                }
                let k = if t < *anchor { 1 } else { (t.0 - anchor.0) / every_ms + 1 };
                k.checked_mul(*every_ms).and_then(|v| v.checked_add(anchor.0)).map(Timestamp)
            }
            Spec::Cron { expr } => Cron::parse(expr)?.next_after(t),
        })
    }

    /// Occurrences `o` with `from ≤ o ≤ to`, counting at most `cap`.
    pub fn count_between(&self, from: Timestamp, to: Timestamp, cap: u64) -> Result<u64, SpecError> {
        if to < from {
            return Ok(0);
        }
        let before = Timestamp(from.0.saturating_sub(1));
        match self {
            Spec::At { at } => Ok(u64::from(from <= *at && *at <= to).min(cap)),
            Spec::Every { every_ms, .. } => Ok(match self.next_after(before)? {
                Some(first) if first <= to => ((to.0 - first.0) / every_ms + 1).min(cap),
                _ => 0,
            }),
            Spec::Cron { expr } => {
                let cron = Cron::parse(expr)?;
                let mut n = 0;
                let mut o = cron.next_after(before);
                while let Some(t) = o {
                    if t > to || n >= cap {
                        break;
                    }
                    n += 1;
                    o = cron.next_after(t);
                }
                Ok(n)
            }
        }
    }

    /// Smallest gap between consecutive occurrences among the first `samples` after `from`.
    pub fn min_gap(&self, from: Timestamp, samples: usize) -> Result<Option<u64>, SpecError> {
        let cron = match self {
            Spec::At { .. } => return Ok(None),
            Spec::Every { every_ms, .. } => return Ok(Some(*every_ms)),
            Spec::Cron { expr } => Cron::parse(expr)?,
        };
        let Some(mut p) = cron.next_after(from) else { return Ok(None) };
        let mut gap: Option<u64> = None;
        for _ in 0..samples {
            let Some(n) = cron.next_after(p) else { break };
            gap = Some(gap.map_or(n.0 - p.0, |g| g.min(n.0 - p.0)));
            p = n;
        }
        Ok(gap)
    }

    /// `once at 2026-09-27T09:00:00Z`, `every 30m`, `cron \`0 9 * * 1-5\` (UTC)`.
    pub fn describe(&self) -> String {
        match self {
            Spec::At { at } => format!("once at {}", format_utc(*at)),
            Spec::Every { every_ms, .. } => format!("every {}", format_duration(*every_ms)),
            Spec::Cron { expr } => format!("cron `{expr}` (UTC)"),
        }
    }
}
