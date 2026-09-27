//! `Custom{ns: "Selfdev", …}` entries and their fold.
//!
//! Folded state (one JSON object):
//! * `published`: sorted hashes; `smoke`: `{hash: {ok, detail}}` (latest per hash);
//! * `channels`: `{channel: hash}` as the log believes them;
//! * `pending`: `{hash, channels, previous}` between `Activated` and `Handoff`, else `null`;
//! * `running`: the hash of the last `Handoff{ok}`; `restored`: the `previous` map the last
//!   `Handoff{rolled_back}` put back;
//! * `restore`: what a rollback would restore now: the pending activation's `previous`, or after
//!   `Handoff{ok}` the channels as they were before that build went live; `null` after a rollback.

use aho_types::Value;
use serde_json::{Map, json};

use crate::builds::Channel;

pub const NS: &str = "Selfdev";
pub const PUBLISHED: &str = "Published";
pub const SMOKE: &str = "Smoke";
pub const ACTIVATED: &str = "Activated";
pub const HANDOFF: &str = "Handoff";

fn initial() -> Value {
    json!({
        "published": [],
        "smoke": {},
        "channels": {},
        "pending": null,
        "running": null,
        "restored": null,
        "restore": null,
    })
}

fn short(h: &str) -> &str {
    h.get(..12).unwrap_or(h)
}

fn obj<'a>(s: &'a mut Value, key: &str) -> Result<&'a mut Map<String, Value>, String> {
    s.get_mut(key)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| format!("the folded Selfdev state has no `{key}` object; the log was not written by this fold"))
}

/// `aho_log::CustomFold` for the `Selfdev` namespace.
pub fn fold(prev: Option<&Value>, name: &str, body: &Value) -> Result<Value, String> {
    let mut s = prev.cloned().unwrap_or_else(initial);
    let hash = body
        .get("hash")
        .and_then(Value::as_str)
        .filter(|h| !h.is_empty())
        .ok_or_else(|| format!("Selfdev/{name} has no `hash`; every Selfdev entry names the build it is about"))?
        .to_string();
    let published = s.get("published").and_then(Value::as_array).is_some_and(|a| a.iter().any(|h| h.as_str() == Some(hash.as_str())));
    match name {
        PUBLISHED => {
            if !published {
                let list =
                    s.get_mut("published").and_then(Value::as_array_mut).ok_or("the folded Selfdev state has no `published` list")?;
                let at = list.partition_point(|h| h.as_str().is_some_and(|h| h < hash.as_str()));
                list.insert(at, Value::String(hash));
            }
        }
        SMOKE => {
            if !published {
                return Err(format!("Smoke for build {} that was never Published; publish it before smoke-booting it", short(&hash)));
            }
            let ok =
                body.get("ok").and_then(Value::as_bool).ok_or_else(|| format!("Smoke for build {} has no boolean `ok`", short(&hash)))?;
            let detail = body.get("detail").cloned().unwrap_or(Value::Null);
            obj(&mut s, "smoke")?.insert(hash, json!({"ok": ok, "detail": detail}));
        }
        ACTIVATED => {
            let smoked = s.get("smoke").and_then(|m| m.get(&hash)).and_then(|r| r.get("ok")) == Some(&Value::Bool(true));
            if !smoked {
                return Err(format!(
                    "Activated build {} without a successful Smoke for it; only a build that passed its smoke boot may be activated",
                    short(&hash)
                ));
            }
            if let Some(p) = s.get("pending").and_then(|p| p.get("hash")).and_then(Value::as_str) {
                return Err(format!(
                    "Activated build {} while build {} is still pending; a Handoff must close that activation first",
                    short(&hash),
                    short(p)
                ));
            }
            if body.get("pending") != Some(&Value::Bool(true)) {
                return Err(format!(
                    "Activated build {} without `pending: true`; an activation stays pending until its Handoff",
                    short(&hash)
                ));
            }
            let channels = body
                .get("channels")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("Activated build {} has no `channels` list", short(&hash)))?;
            for c in channels {
                if c.as_str().and_then(Channel::parse).is_none() {
                    return Err(format!(
                        "Activated build {} names unknown channel {c}; channels are current, stable, canary",
                        short(&hash)
                    ));
                }
            }
            let previous = body
                .get("previous")
                .filter(|p| p.is_object())
                .ok_or_else(|| format!("Activated build {} has no `previous` map", short(&hash)))?;
            let pending = json!({"hash": hash, "channels": channels, "previous": previous});
            s["restore"] = previous.clone();
            s["pending"] = pending;
        }
        HANDOFF => {
            let pending = s.get("pending").cloned().unwrap_or(Value::Null);
            match pending.get("hash").and_then(Value::as_str) {
                None => {
                    return Err(format!(
                        "Handoff for build {} without a pending activation; a Handoff only closes an Activated entry",
                        short(&hash)
                    ));
                }
                Some(p) if p != hash => {
                    return Err(format!("Handoff for build {} while build {} is the pending activation", short(&hash), short(p)));
                }
                Some(_) => {}
            }
            let channels: Vec<String> = pending
                .get("channels")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let previous = pending.get("previous").cloned().unwrap_or_else(|| json!({}));
            match body.get("outcome").and_then(Value::as_str) {
                Some("ok") => {
                    let map = obj(&mut s, "channels")?;
                    for c in &channels {
                        map.insert(c.clone(), Value::String(hash.clone()));
                    }
                    s["running"] = Value::String(hash);
                    s["restore"] = previous;
                }
                Some("rolled_back") => {
                    let map = obj(&mut s, "channels")?;
                    for c in &channels {
                        match previous.get(c).and_then(Value::as_str) {
                            Some(h) => map.insert(c.clone(), Value::String(h.to_string())),
                            None => map.remove(c),
                        };
                    }
                    s["restored"] = previous;
                    s["restore"] = Value::Null;
                }
                other => {
                    return Err(format!("Handoff for build {} has outcome {other:?}; it must be \"ok\" or \"rolled_back\"", short(&hash)));
                }
            }
            s["pending"] = Value::Null;
        }
        other => return Err(format!("unknown Selfdev entry `{other}`; this fold knows Published, Smoke, Activated and Handoff")),
    }
    Ok(s)
}
