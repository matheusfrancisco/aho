//! Okapi BM25 (k1 = 1.2, b = 0.75) over entry text + tags, with a code-aware tokenizer:
//! identifiers are indexed whole *and* split into parts, so an exact identifier is a rare,
//! high-IDF term while its pieces still match prose.

use std::collections::{BTreeMap, BTreeSet};

pub const K1: f32 = 1.2;
pub const B: f32 = 0.75;

const STOPWORDS: &[&str] = &[
    "a", "about", "after", "all", "also", "am", "an", "and", "any", "are", "as", "at", "be", "because", "been", "before", "being", "but",
    "by", "can", "could", "did", "do", "does", "doing", "for", "from", "had", "has", "have", "having", "he", "her", "here", "his", "how",
    "i", "if", "in", "into", "is", "it", "its", "just", "me", "my", "no", "not", "of", "on", "or", "our", "out", "she", "should", "so",
    "some", "such", "than", "that", "the", "their", "them", "then", "there", "these", "they", "this", "those", "to", "too", "up", "very",
    "was", "we", "were", "what", "when", "where", "which", "while", "who", "why", "will", "with", "would", "you", "your",
];

pub fn is_stopword(w: &str) -> bool {
    STOPWORDS.contains(&w)
}

/// `parse_config_v2` → `parse`, `config`, `v2`; `HTTPServer` → `HTTP`, `Server`; `parseConfig` → `parse`, `Config`.
fn split_identifier(w: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for piece in w.split('_').filter(|p| !p.is_empty()) {
        let cs: Vec<char> = piece.chars().collect();
        let mut start = 0;
        for i in 1..cs.len() {
            let (p, c) = (cs[i - 1], cs[i]);
            let next_lower = cs.get(i + 1).is_some_and(|n| n.is_lowercase());
            if (p.is_lowercase() && c.is_uppercase()) || (p.is_uppercase() && c.is_uppercase() && next_lower) {
                parts.push(cs[start..i].iter().collect());
                start = i;
            }
        }
        parts.push(cs[start..].iter().collect());
    }
    parts
}

/// Lowercased index terms. Compounds (`src/config.rs`, `aho_cap::plan`) are kept whole too.
pub fn terms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in text.split(|c: char| !(c.is_alphanumeric() || "_-./:".contains(c))) {
        let chunk = chunk.trim_matches(|c: char| !c.is_alphanumeric());
        if chunk.is_empty() {
            continue;
        }
        let words: Vec<&str> = chunk.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).collect();
        if words.len() > 1 {
            out.push(chunk.to_lowercase());
        }
        for w in words {
            let lw = w.to_lowercase();
            let parts = split_identifier(w);
            if parts.len() > 1 {
                out.push(lw);
                out.extend(parts.iter().map(|p| p.to_lowercase()).filter(|p| !is_stopword(p)));
            } else if !is_stopword(&lw) {
                out.push(lw);
            }
        }
    }
    out
}

/// Scores `docs` (id, pre-tokenized terms) against `query`; only positive scores, best first,
/// ties by id.
pub fn rank(query: &str, docs: &[(u64, Vec<String>)]) -> Vec<(u64, f32)> {
    let q: BTreeSet<String> = terms(query).into_iter().collect();
    if q.is_empty() || docs.is_empty() {
        return Vec::new();
    }
    let n = docs.len() as f32;
    let avgdl = (docs.iter().map(|(_, t)| t.len()).sum::<usize>() as f32 / n).max(1.0);
    let tfs: Vec<BTreeMap<&str, f32>> = docs
        .iter()
        .map(|(_, ts)| {
            let mut m: BTreeMap<&str, f32> = BTreeMap::new();
            for t in ts.iter().filter(|t| q.contains(*t)) {
                *m.entry(t.as_str()).or_insert(0.0) += 1.0;
            }
            m
        })
        .collect();
    let idf: BTreeMap<&str, f32> = q
        .iter()
        .map(|t| {
            let df = tfs.iter().filter(|m| m.contains_key(t.as_str())).count() as f32;
            (t.as_str(), (1.0 + (n - df + 0.5) / (df + 0.5)).ln())
        })
        .collect();
    let mut out: Vec<(u64, f32)> = docs
        .iter()
        .zip(&tfs)
        .filter_map(|((id, ts), tf)| {
            let dl = ts.len() as f32;
            let s: f32 = tf.iter().map(|(t, f)| idf[t] * f * (K1 + 1.0) / (f + K1 * (1.0 - B + B * dl / avgdl))).sum();
            (s > 0.0).then_some((*id, s))
        })
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docs(texts: &[&str]) -> Vec<(u64, Vec<String>)> {
        texts.iter().enumerate().map(|(i, t)| (i as u64 + 1, terms(t))).collect()
    }

    #[test]
    fn identifiers_are_indexed_whole_and_in_parts() {
        let t = terms("Fix parse_config_v2 in src/config.rs via HTTPServer::bindAddr");
        for want in
            ["parse_config_v2", "parse", "config", "v2", "src/config.rs", "httpserver", "http", "server", "bindaddr", "bind", "addr", "fix"]
        {
            assert!(t.contains(&want.to_string()), "{want} missing from {t:?}");
        }
        assert!(!t.contains(&"in".to_string()), "stopwords dropped");
    }

    #[test]
    fn identifier_heavy_exact_match_ranks_first() {
        let d = docs(&[
            "the parser reads the config file and reports errors in the config",
            "config parsing is slow on large config files; parse config lazily",
            "parse_config_v2 fails when the TOML has trailing commas",
            "unrelated note about deployment windows",
        ]);
        let r = rank("why does parse_config_v2 fail on this config", &d);
        assert_eq!(r[0].0, 3, "{r:?}");
        assert!(r.iter().all(|(id, _)| *id != 4), "no shared terms, no score");
    }

    #[test]
    fn length_normalization_and_saturation() {
        // same single match; the shorter document scores higher (b = 0.75)
        let d = docs(&["cache eviction", "cache eviction policy notes with many other words filling the document body"]);
        let r = rank("eviction", &d);
        assert_eq!(r.iter().map(|x| x.0).collect::<Vec<_>>(), vec![1, 2]);
        // tf saturates: ten repeats score more than one occurrence, but not even twice as much
        let d = docs(&["token", "token token token token token token token token token token", "other", "words"]);
        let r = rank("token", &d);
        let one = r.iter().find(|x| x.0 == 1).unwrap().1;
        let ten = r.iter().find(|x| x.0 == 2).unwrap().1;
        assert!(ten > one && ten < 2.0 * one, "ten = {ten}, one = {one}");
    }

    #[test]
    fn empty_query_or_corpus_scores_nothing() {
        assert!(rank("the of and", &docs(&["the of and a"])).is_empty());
        assert!(rank("anything", &[]).is_empty());
    }
}
