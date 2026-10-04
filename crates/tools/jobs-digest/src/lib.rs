//! `jobs-digest`: job outcomes that arrive close together reach the model as one notice.
//! Opt-in like telemetry (`aho enable jobs-digest`); `jobs_digest.window_ms` (default 2000) is
//! the batching window.
//!
//! One `Registrar::delivery` callback. The first delivery offered while no batch is open starts
//! one and is deferred until `window_ms` after that offer; every delivery offered before then
//! joins it and is deferred to the same instant, so the session offers the whole batch again at
//! one delivery point. There the first member offered (the lead) carries the digest: a header,
//! then each member's outcome under its own heading (`prompts/digest.md`). Every other member
//! offered there is delivered as a one-line pointer to the lead.
//!
//! Replay-honest: each op still gets its own `JobDelivered`, with its own status and then its
//! `OpDone`, so the fold closes every job exactly once; each entry holds exactly the text the
//! model was shown, and the pointers come right after the digest they name. A batch of one is
//! delivered unchanged. A member not offered with its lead (a capability ahead of this one
//! deferred it) is not in the digest and is batched again when it comes back. A capability after
//! this one that defers the lead splits the batch: the pointers are logged now, the lead later,
//! with its own outcome only.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_log::OpStatus;
use aho_types::{ContentBlock, OpId, Value};
use serde_json::json;

pub const WINDOW_KEY: &str = "jobs_digest.window_ms";
pub const DEFAULT_WINDOW_MS: u64 = 2_000;

/// `prompts/digest.md`: the digest's fixed wording, one `key: template` per line.
const TEMPLATES: &str = include_str!("../prompts/digest.md");

fn template(key: &str) -> &'static str {
    TEMPLATES.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(": ")).unwrap_or("")
}

#[derive(Clone, Debug, PartialEq)]
pub struct DigestConfig {
    pub window: Duration,
}

impl Config for DigestConfig {
    const PREFIX: &'static str = "jobs_digest";

    fn keys() -> Vec<ConfigKey> {
        vec![ConfigKey::new(
            WINDOW_KEY,
            Ty::Int,
            Some(json!(DEFAULT_WINDOW_MS)),
            "Milliseconds after the first finished job is offered in which later ones join it; the batch then reaches the model as one digest notice.",
        )]
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let ms = match l.get(WINDOW_KEY) {
            None | Some(Value::Null) => DEFAULT_WINDOW_MS,
            Some(v) => v.as_u64().filter(|n| *n > 0).ok_or(ConfigError::Type { key: WINDOW_KEY.into(), expected: "a positive integer" })?,
        };
        Ok(DigestConfig { window: Duration::from_millis(ms) })
    }
}

// ───────────────────────────── batching ─────────────────────────────

struct Member {
    op: OpId,
    tool: String,
    status: OpStatus,
    content: Vec<ContentBlock>,
}

/// Deliveries deferred to the same instant.
struct Batch {
    until: Instant,
    /// In the order they joined; emptied when the batch is released.
    members: Vec<Member>,
    /// Set when the batch is released: the member carrying the digest, and the members folded
    /// into it that have not been offered since.
    lead: Option<(OpId, Vec<OpId>)>,
}

impl Batch {
    fn holds(&self, op: OpId) -> bool {
        match &self.lead {
            None => self.members.iter().any(|m| m.op == op),
            Some((_, folded)) => folded.contains(&op),
        }
    }

    /// `v`, a member, is offered after the hold: the first becomes the lead and carries every
    /// member offered with it; each of those then gets a pointer to the lead.
    fn release(&mut self, v: &JobView, window: Duration) -> Delivery {
        if let Some((lead, folded)) = &mut self.lead {
            folded.retain(|op| *op != v.op);
            return Delivery::Replace(vec![ContentBlock::text(template("pointer").replace("{lead}", &lead.0.to_string()))]);
        }
        let here: Vec<Member> = std::mem::take(&mut self.members).into_iter().filter(|m| m.op == v.op || v.with.contains(&m.op)).collect();
        self.lead = Some((v.op, here.iter().map(|m| m.op).filter(|op| *op != v.op).collect()));
        if here.len() < 2 { Delivery::Keep } else { Delivery::Replace(render(&here, window)) }
    }

    fn done(&self) -> bool {
        matches!(&self.lead, Some((_, folded)) if folded.is_empty())
    }
}

/// The batching state behind the capability's delivery callback.
pub struct Digest {
    window: Duration,
    batches: Mutex<Vec<Batch>>,
}

impl Digest {
    pub fn new(window: Duration) -> Digest {
        Digest { window, batches: Mutex::default() }
    }

    /// The decision for one delivery offered at `now`.
    pub fn decide(&self, v: &JobView, now: Instant) -> Delivery {
        let mut batches = self.batches.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(i) = batches.iter().position(|b| b.holds(v.op)) {
            let b = &mut batches[i];
            if b.lead.is_none() && now < b.until {
                return Delivery::Defer { until: b.until };
            }
            let d = b.release(v, self.window);
            if b.done() {
                batches.remove(i);
            }
            return d;
        }
        let member = Member { op: v.op, tool: v.tool.clone(), status: v.status.clone(), content: v.content.clone() };
        let until = match batches.iter_mut().find(|b| b.lead.is_none() && now < b.until) {
            Some(b) => {
                b.members.push(member);
                b.until
            }
            None => {
                let until = now + self.window;
                batches.push(Batch { until, members: vec![member], lead: None });
                until
            }
        };
        Delivery::Defer { until }
    }
}

/// The header, then each member's heading and own content blocks. Headings carry their own line
/// breaks: an adapter may join adjacent text blocks with nothing between them.
fn render(members: &[Member], window: Duration) -> Vec<ContentBlock> {
    let header = template("digest").replace("{n}", &members.len().to_string()).replace("{window}", &window.as_millis().to_string());
    let mut out = vec![ContentBlock::text(header)];
    for m in members {
        let word = match m.status {
            OpStatus::Completed { .. } => "completed",
            OpStatus::Failed { .. } => "failed",
            OpStatus::Cancelled { .. } => "cancelled",
            OpStatus::Uncertain { .. } => "uncertain",
        };
        let heading = template("member").replace("{op}", &m.op.0.to_string()).replace("{tool}", &m.tool).replace("{status}", word);
        out.push(ContentBlock::text(format!("\n\n{heading}\n")));
        out.extend(m.content.iter().cloned());
    }
    out
}

// ───────────────────────────── capability ─────────────────────────────

pub struct JobsDigest {
    digest: Arc<Digest>,
}

impl Capability for JobsDigest {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = DigestConfig;

    fn build(_: (), cfg: DigestConfig) -> Result<Self, BuildError> {
        Ok(JobsDigest { digest: Arc::new(Digest::new(cfg.window)) })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        let digest = self.digest.clone();
        r.delivery(Arc::new(move |v: &JobView| digest.decide(v, Instant::now())));
    }
}

declare_capability!(
    JobsDigest,
    "jobs-digest",
    kind = Kind::Hook,
    provides = [SeamId("hooks")],
    requires = [],
    tools = &[],
    doc = "Job outcomes that arrive within jobs_digest.window_ms reach the model as one digest notice; each job keeps its own logged delivery. Opt-in."
);

#[cfg(test)]
mod tests;
