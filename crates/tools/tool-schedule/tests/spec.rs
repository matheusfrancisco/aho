//! Fire-time math: RFC 3339, intervals, 5-field cron in UTC (no DST), next-fire and counting.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use aho_types::Timestamp;
use tool_schedule::spec::{DAY, HOUR, MINUTE, civil_from_days, days_from_civil, format_duration};
use tool_schedule::{Cron, Spec, format_utc, parse_duration, parse_rfc3339};

fn t(s: &str) -> Timestamp {
    parse_rfc3339(s).unwrap()
}

fn next(expr: &str, from: &str) -> Option<String> {
    Cron::parse(expr).unwrap().next_after(t(from)).map(format_utc)
}

#[test]
fn rfc3339_parses_zones_and_fractions() {
    assert_eq!(t("1970-01-01T00:00:00Z"), Timestamp(0));
    let y2k = Timestamp(946_684_800_000);
    assert_eq!(t("2000-01-01T00:00:00Z"), y2k);
    assert_eq!(t("2000-01-01T02:00:00+02:00"), y2k);
    assert_eq!(t("1999-12-31T19:00:00-05:00"), y2k);
    assert_eq!(t("2000-01-01t00:00:00.5z"), Timestamp(y2k.0 + 500));
    assert_eq!(t("2000-01-01 00:00:00.123456Z"), Timestamp(y2k.0 + 123), "digits past milliseconds are truncated");
    assert_eq!(t("  2028-02-29T23:59:59Z "), Timestamp((days_from_civil(2028, 2, 29) as u64 + 1) * DAY - 1000));
}

#[test]
fn rfc3339_rejects_what_is_not_a_real_instant() {
    for bad in [
        "2026-02-29T00:00:00Z",
        "2026-04-31T00:00:00Z",
        "2026-13-01T00:00:00Z",
        "2026-00-10T00:00:00Z",
        "2026-01-01T24:00:00Z",
        "2026-01-01T00:60:00Z",
        "2026-01-01T00:00:60Z",
        "2026-01-01T00:00:00",
        "2026-01-01T00:00Z",
        "2026-01-01T00:00:00+0200",
        "2026-01-01T00:00:00+24:00",
        "2026-01-01T00:00:00.Z",
        "1969-12-31T23:59:59Z",
        "tomorrow at nine",
    ] {
        assert!(parse_rfc3339(bad).is_err(), "{bad} should be refused");
    }
    let e = parse_rfc3339("2026-02-29T00:00:00Z").unwrap_err().to_string();
    assert!(e.contains("does not exist"), "{e}");
}

#[test]
fn utc_formatting_round_trips_and_civil_math_is_exact() {
    assert_eq!(format_utc(Timestamp(946_684_800_000)), "2000-01-01T00:00:00Z");
    assert_eq!(format_utc(Timestamp(946_684_800_042)), "2000-01-01T00:00:00.042Z");
    for ms in [0u64, 1, 59_999, 951_782_400_000, 1_709_208_000_000, 4_102_444_800_000, 253_402_300_799_000] {
        assert_eq!(parse_rfc3339(&format_utc(Timestamp(ms))).unwrap(), Timestamp(ms));
    }
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    for z in (-800_000i64..800_000).step_by(997) {
        let (y, m, d) = civil_from_days(z);
        assert_eq!(days_from_civil(y, m, d), z, "{y}-{m}-{d}");
    }
}

#[test]
fn intervals_parse_with_units_and_refuse_nonsense() {
    assert_eq!(parse_duration("30m").unwrap(), 30 * MINUTE);
    assert_eq!(parse_duration("1h30m").unwrap(), 90 * MINUTE);
    assert_eq!(parse_duration("1h 30m").unwrap(), 90 * MINUTE);
    assert_eq!(parse_duration("90s").unwrap(), 90_000);
    assert_eq!(parse_duration("2d").unwrap(), 2 * DAY);
    assert_eq!(parse_duration("1w").unwrap(), 7 * DAY);
    assert_eq!(parse_duration("10 minutes").unwrap(), 10 * MINUTE);
    for bad in ["", "0m", "5", "m", "5x", "1h-2m", "99999999999999999999s"] {
        assert!(parse_duration(bad).is_err(), "{bad:?} should be refused");
    }
    assert_eq!(format_duration(90 * MINUTE), "1h30m");
    assert_eq!(format_duration(DAY + 2 * HOUR), "1d2h");
    assert_eq!(format_duration(90_000), "1m30s");
}

#[test]
fn cron_refuses_malformed_expressions_with_the_field_named() {
    for (bad, needle) in [
        ("* * * *", "4 fields"),
        ("* * * * * *", "6 fields"),
        ("60 * * * *", "minute must be 0–59"),
        ("* 24 * * *", "hour must be 0–23"),
        ("* * 0 * *", "day of month must be 1–31"),
        ("* * * 13 *", "month must be 1–12"),
        ("* * * * 8", "day of week must be 0–7"),
        ("5-1 * * * *", "runs backwards"),
        ("*/0 * * * *", "positive number"),
        ("* * * FOO *", "not a valid month"),
        ("@reboot", "unknown shortcut"),
    ] {
        let e = Cron::parse(bad).unwrap_err().to_string();
        assert!(e.contains(needle), "{bad}: {e}");
    }
}

#[test]
fn cron_next_fire_is_strictly_after_and_minute_aligned() {
    assert_eq!(next("*/15 * * * *", "2026-09-26T10:07:30Z").as_deref(), Some("2026-09-26T10:15:00Z"));
    assert_eq!(next("*/15 * * * *", "2026-09-26T10:15:00Z").as_deref(), Some("2026-09-26T10:30:00Z"), "an exact match is not 'after'");
    assert_eq!(next("*/15 * * * *", "2026-09-26T10:15:00.001Z").as_deref(), Some("2026-09-26T10:30:00Z"));
    assert_eq!(next("*/15 * * * *", "2026-09-26T23:59:00Z").as_deref(), Some("2026-09-27T00:00:00Z"));
    assert_eq!(next("5,10-20/5 * * * *", "2026-09-26T10:20:00Z").as_deref(), Some("2026-09-26T11:05:00Z"));
}

#[test]
fn cron_crosses_month_year_and_leap_boundaries() {
    assert_eq!(next("0 0 1 1 *", "2026-12-31T23:59:00Z").as_deref(), Some("2027-01-01T00:00:00Z"));
    assert_eq!(next("59 23 31 12 *", "2026-12-31T23:59:00Z").as_deref(), Some("2027-12-31T23:59:00Z"));
    assert_eq!(next("0 0 29 2 *", "2026-03-01T00:00:00Z").as_deref(), Some("2028-02-29T00:00:00Z"));
    assert_eq!(next("0 0 29 2 *", "2096-03-01T00:00:00Z").as_deref(), Some("2104-02-29T00:00:00Z"), "2100 is not a leap year");
    assert_eq!(next("0 0 31 * *", "2026-04-01T00:00:00Z").as_deref(), Some("2026-05-31T00:00:00Z"), "April has no 31st");
    assert_eq!(next("0 12 1 JAN-MAR *", "2026-03-02T00:00:00Z").as_deref(), Some("2027-01-01T12:00:00Z"));
}

#[test]
fn cron_day_fields_follow_vixie_or_and_rules() {
    // 2026-09-26 is a Saturday. Both day fields restricted: either matches.
    assert_eq!(next("0 0 13 * 5", "2026-09-26T00:00:00Z").as_deref(), Some("2026-10-02T00:00:00Z"), "Friday before the 13th");
    assert_eq!(next("0 0 13 * 5", "2026-10-10T00:00:00Z").as_deref(), Some("2026-10-13T00:00:00Z"), "the 13th before Friday");
    // A `*`-prefixed day field makes both required: odd days that are Fridays (Oct 2 is even, Oct 9 odd).
    assert_eq!(next("0 0 */2 * 5", "2026-09-26T00:00:00Z").as_deref(), Some("2026-10-09T00:00:00Z"));
    // Sunday is 0, 7 and SUN.
    for e in ["0 12 * * 0", "0 12 * * 7", "0 12 * * SUN", "0 12 * * sun"] {
        assert_eq!(next(e, "2026-09-26T13:00:00Z").as_deref(), Some("2026-09-27T12:00:00Z"), "{e}");
    }
    assert_eq!(next("0 9 * * MON-FRI", "2026-09-26T00:00:00Z").as_deref(), Some("2026-09-28T09:00:00Z"));
    assert_eq!(Cron::parse("@daily").unwrap(), Cron::parse("0 0 * * *").unwrap());
    assert_eq!(Cron::parse("@weekly").unwrap(), Cron::parse("0 0 * * 0").unwrap());
}

#[test]
fn cron_that_names_no_real_date_never_fires() {
    assert_eq!(next("0 0 30 2 *", "2026-01-01T00:00:00Z"), None);
    assert_eq!(next("0 0 31 4,6,9,11 *", "2026-01-01T00:00:00Z"), None);
}

#[test]
fn utc_has_no_daylight_saving_gaps_or_folds() {
    // US DST starts 2026-03-08, EU 2026-03-29, EU ends 2026-10-25, US ends 2026-11-01: in UTC nothing moves.
    let spec = Spec::Cron { expr: "30 2 * * *".into() };
    let mut at = t("2026-03-01T00:00:00Z");
    let mut prev: Option<Timestamp> = None;
    while at < t("2026-11-10T00:00:00Z") {
        let n = spec.next_after(at).unwrap().unwrap();
        assert!(format_utc(n).ends_with("T02:30:00Z"), "{}", format_utc(n));
        if let Some(p) = prev {
            assert_eq!(n.0 - p.0, DAY, "{} → {}", format_utc(p), format_utc(n));
        }
        prev = Some(n);
        at = n;
    }
    // an hour that does not exist in New York local time is an ordinary UTC hour
    assert_eq!(next("0 2 8 3 *", "2026-03-01T00:00:00Z").as_deref(), Some("2026-03-08T02:00:00Z"));
}

#[test]
fn every_counts_from_its_anchor() {
    let a = t("2026-09-26T10:00:00Z");
    let s = Spec::Every { every_ms: 30 * MINUTE, anchor: a };
    let at = |m: u64| Timestamp(a.0 + m * MINUTE);
    assert_eq!(s.next_after(a).unwrap(), Some(at(30)), "the anchor itself is not an occurrence");
    assert_eq!(s.next_after(at(30)).unwrap(), Some(at(60)));
    assert_eq!(s.next_after(at(45)).unwrap(), Some(at(60)));
    assert_eq!(s.next_after(Timestamp(a.0 - 1)).unwrap(), Some(at(30)));
    assert_eq!(s.count_between(at(30), at(120), 100).unwrap(), 4);
    assert_eq!(s.count_between(at(30), at(30), 100).unwrap(), 1);
    assert_eq!(s.count_between(at(31), at(59), 100).unwrap(), 0);
    assert_eq!(s.count_between(at(30), at(10_000), 5).unwrap(), 5, "capped");
    assert_eq!(s.describe(), "every 30m");
}

#[test]
fn at_fires_once_and_cron_counts_between_bounds() {
    let when = t("2026-09-27T09:00:00Z");
    let s = Spec::At { at: when };
    assert_eq!(s.next_after(Timestamp(when.0 - 1)).unwrap(), Some(when));
    assert_eq!(s.next_after(when).unwrap(), None);
    assert_eq!(s.count_between(when, Timestamp(when.0 + DAY), 10).unwrap(), 1);
    assert_eq!(s.describe(), "once at 2026-09-27T09:00:00Z");

    let c = Spec::Cron { expr: "*/10 * * * *".into() };
    assert_eq!(c.count_between(t("2026-09-26T10:00:00Z"), t("2026-09-26T10:59:59Z"), 100).unwrap(), 6);
    assert_eq!(c.count_between(t("2026-09-26T10:00:00Z"), t("2026-09-26T10:59:59Z"), 4).unwrap(), 4);
    assert_eq!(c.min_gap(t("2026-09-26T10:00:00Z"), 8).unwrap(), Some(10 * MINUTE));
    let weekdays = Spec::Cron { expr: "0 9 * * 1-5".into() };
    assert_eq!(weekdays.min_gap(t("2026-09-26T00:00:00Z"), 8).unwrap(), Some(DAY));
}
