//! `aho gc --dry-run` output: one table row per orphan (owner, kind, age, size, path), then what
//! was kept and why, owners waiting for their second scan, and notes (doc 12 message voice).

use crate::{GcOutcome, GcReport, Orphan};

pub(crate) fn age(ms: Option<u64>) -> String {
    let Some(ms) = ms else { return "-".into() };
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3_600 => format!("{}m", s / 60),
        3_600..86_400 => format!("{}h", s / 3_600),
        _ => format!("{}d", s / 86_400),
    }
}

pub(crate) fn size(bytes: Option<u64>) -> String {
    let Some(b) = bytes else { return "-".into() };
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if b < 1024 {
        return format!("{b} B");
    }
    let mut v = b as f64 / 1024.0;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

fn table(items: &[Orphan]) -> String {
    let rows: Vec<[String; 5]> =
        items.iter().map(|o| [o.owner.clone(), o.kind.as_str().to_string(), age(o.age_ms), size(o.size), o.path.clone()]).collect();
    let head = ["OWNER", "KIND", "AGE", "SIZE", "PATH"];
    let mut w = head.map(str::len);
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }
    let line = |cells: [&str; 5]| {
        let mut s = String::new();
        for (i, c) in cells.iter().enumerate() {
            if i == 4 {
                s.push_str(c);
            } else if i == 3 {
                s.push_str(&format!("{c:>width$}  ", width = w[i]));
            } else {
                s.push_str(&format!("{c:<width$}  ", width = w[i]));
            }
        }
        s.trim_end().to_string() + "\n"
    };
    let mut out = line(head);
    for r in &rows {
        out.push_str(&line([&r[0], &r[1], &r[2], &r[3], &r[4]]));
    }
    out
}

pub fn render_report(r: &GcReport) -> String {
    let mut out = String::new();
    if r.items.is_empty() {
        out.push_str("nothing to reclaim\n");
    } else {
        out.push_str(&table(&r.items));
        let total: u64 = r.items.iter().filter_map(|o| o.size).sum();
        out.push_str(&format!("{} orphan(s), {} on disk; `aho gc` reclaims them\n", r.items.len(), size(Some(total))));
    }
    for p in &r.pending {
        out.push_str(&format!(
            "pending: session {} lost its process (pid {}) on this scan; its {} running child(ren) are reaped if it is still gone on the next scan\n",
            p.owner, p.identity.pid, p.children
        ));
    }
    for k in &r.kept {
        out.push_str(&format!("kept: {} {}: {}\n", k.kind.as_str(), k.path, k.why));
    }
    for n in &r.notes {
        out.push_str(&format!("note: {n}\n"));
    }
    out
}

pub fn render_outcome(o: &GcOutcome) -> String {
    let mut out = format!("reclaimed {} item(s), {} freed\n", o.removed.len(), size(Some(o.freed)));
    for (item, why) in &o.skipped {
        out.push_str(&format!("skipped: {} {}: {why}\n", item.kind.as_str(), item.path));
    }
    for (item, why) in &o.failed {
        out.push_str(&format!("failed: {} {}: {why}\n", item.kind.as_str(), item.path));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_and_sizes_read_at_a_glance() {
        assert_eq!(
            [age(Some(59_999)), age(Some(60_000)), age(Some(7_200_000)), age(Some(3 * 86_400_000)), age(None)],
            ["59s", "1m", "2h", "3d", "-"]
        );
        assert_eq!([size(Some(1023)), size(Some(1536)), size(Some(5 << 20)), size(None)], ["1023 B", "1.5 KiB", "5.0 MiB", "-"]);
    }
}
