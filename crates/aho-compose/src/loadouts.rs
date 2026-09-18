//! `aho loadout save|load|diff|list <name>`: frozen resolved tables under `~/.aho/loadouts/`.
//!
//! A saved loadout is a TOML file of quoted dotted keys (`"seams.world" = "host"`), one per
//! resolved key, plus `disable = [..]`. Loading it yields an `Arg`-rank layer whose origins point
//! at lines of that file, so `explain` still says exactly where each value came from. Secret
//! values are never frozen: only their key names are listed in a comment.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use aho_config::{FileSnapshot, Layer, Loadout};
use aho_types::Value;

use crate::ComposeError;
use crate::inputs::{read_optional, validate_name, write_atomic};

#[derive(Clone, Debug, PartialEq)]
pub struct Saved {
    pub name: String,
    pub values: BTreeMap<String, Value>,
    pub disabled: Vec<String>,
}

pub fn dir(home: &Path) -> PathBuf {
    home.join(".aho").join("loadouts")
}

pub fn path(home: &Path, name: &str) -> Result<PathBuf, ComposeError> {
    validate_name("loadout", name)?;
    Ok(dir(home).join(format!("{name}.toml")))
}

/// Freezes the resolved table. Returns the file written.
pub fn save(home: &Path, name: &str, loadout: &Loadout) -> Result<PathBuf, ComposeError> {
    let file = path(home, name)?;
    let err = |e: String| ComposeError::Loadout { path: file.clone(), err: e };
    let mut doc = toml_edit::DocumentMut::new();
    let mut skipped = Vec::new();
    for (key, sourced) in &loadout.config {
        if loadout.secrets.contains_key(key) {
            skipped.push(key.as_str());
            continue;
        }
        if let Some(v) = to_toml(&sourced.value) {
            doc.insert(key, toml_edit::Item::Value(v));
        }
    }
    let mut disabled: Vec<&str> = loadout.disabled.iter().map(|(d, _)| d.as_str()).collect();
    disabled.sort();
    disabled.dedup();
    if !disabled.is_empty() {
        doc.insert("disable", toml_edit::value(disabled.iter().copied().collect::<toml_edit::Array>()));
    }
    let mut text = format!("# aho loadout `{name}` (loadout {})\n", loadout.hash.short());
    if !skipped.is_empty() {
        text.push_str(&format!("# secrets are never frozen; set them again: {}\n", skipped.join(", ")));
    }
    text.push_str(&doc.to_string());
    write_atomic(&file, &text).map_err(err)?;
    Ok(file)
}

/// Parses a saved loadout for `diff`.
pub fn read(home: &Path, name: &str) -> Result<Saved, ComposeError> {
    let file = path(home, name)?;
    let text = read_text(&file)?;
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| ComposeError::Loadout { path: file.clone(), err: e.to_string() })?;
    let mut values = BTreeMap::new();
    let mut disabled = Vec::new();
    for (k, v) in table {
        if k == "disable" {
            disabled = v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
            continue;
        }
        values.insert(k, serde_json::to_value(&v).unwrap_or(Value::Null));
    }
    Ok(Saved { name: name.to_string(), values, disabled })
}

/// The saved table as an `Arg`-rank layer (`Inputs::with_layer`): it beats files and env the
/// way flags do, and never pretends to be a runtime (`aho apply`) change.
pub fn load(home: &Path, name: &str) -> Result<FileSnapshot, ComposeError> {
    let file = path(home, name)?;
    let text = read_text(&file)?;
    Ok(FileSnapshot { path: file.display().to_string(), layer: Layer::Arg, text })
}

#[allow(clippy::disallowed_methods, reason = "authority: compose-list-loadouts")]
pub fn list(home: &Path) -> Result<Vec<String>, ComposeError> {
    let d = dir(home);
    let entries = match std::fs::read_dir(&d) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(ComposeError::Io { path: d, err: e.to_string() }),
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| n.strip_suffix(".toml").map(str::to_string))
        .collect();
    names.sort();
    Ok(names)
}

/// `(key, in a, in b)` for every key whose value differs; `disable` compares as a set.
pub fn diff(a: &Saved, b: &Saved) -> Vec<(String, Option<Value>, Option<Value>)> {
    let keys: BTreeSet<&String> = a.values.keys().chain(b.values.keys()).collect();
    let mut out: Vec<(String, Option<Value>, Option<Value>)> = keys
        .into_iter()
        .filter_map(|k| {
            let (x, y) = (a.values.get(k).cloned(), b.values.get(k).cloned());
            (x != y).then(|| (k.clone(), x, y))
        })
        .collect();
    let set = |s: &Saved| s.disabled.iter().cloned().collect::<BTreeSet<String>>();
    if set(a) != set(b) {
        let arr = |s: &Saved| Some(Value::from(set(s).into_iter().collect::<Vec<String>>()));
        out.push(("disable".into(), arr(a), arr(b)));
    }
    out
}

fn read_text(file: &Path) -> Result<String, ComposeError> {
    read_optional(file)?.ok_or_else(|| ComposeError::Loadout { path: file.to_path_buf(), err: "no saved loadout by that name".into() })
}

fn to_toml(v: &Value) -> Option<toml_edit::Value> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => (*b).into(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.into(),
            None => n.as_f64()?.into(),
        },
        Value::String(s) => s.as_str().into(),
        Value::Array(a) => toml_edit::Value::Array(a.iter().filter_map(to_toml).collect()),
        Value::Object(m) => {
            let mut t = toml_edit::InlineTable::new();
            for (k, x) in m {
                if let Some(x) = to_toml(x) {
                    t.insert(k.as_str(), x);
                }
            }
            toml_edit::Value::InlineTable(t)
        }
    })
}
