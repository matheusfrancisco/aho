//! `aho catalog config`: every settable key, generated from the schema the capabilities declare (03).

use aho_config::{ConfigKey, Schema};
use aho_types::Value;

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn ty(k: &ConfigKey) -> String {
    let base = match serde_json::to_value(k.ty) {
        Ok(Value::String(s)) => s,
        _ => format!("{:?}", k.ty).to_lowercase(),
    };
    if k.secret { format!("{base} (secret)") } else { base }
}

fn default(k: &ConfigKey) -> String {
    match &k.default {
        None => "—".into(),
        Some(_) if k.secret => "(secret)".into(),
        Some(v) => format!("`{}`", cell(&v.to_string())),
    }
}

/// Markdown table, one row per key in path order. The schema hash heads the file so the
/// doc-freshness gate is "committed catalog hash == schema hash of this binary".
pub fn catalog_config(schema: &Schema) -> String {
    let mut out = format!(
        "# Configuration catalog\n\n<!-- schema-hash: {} -->\n\n{} keys. Set any of them in `~/.aho/config.toml`, `./aho.toml`, the listed env var, or `--set key=value`.\n\n",
        schema.hash().to_hex(),
        schema.keys.len()
    );
    out.push_str("| key | type | default | env | description |\n|---|---|---|---|---|\n");
    for k in schema.keys.values() {
        out.push_str(&format!("| `{}` | {} | {} | `{}` | {} |\n", k.path, ty(k), default(k), k.env_var(), cell(&k.doc)));
    }
    out
}
