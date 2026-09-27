//! The context producer behind `Registrar::context`: once per fresh user turn it retrieves for
//! the latest user prompt and hands the session a Snapshot text that passed suppression.
//!
//! Never blocks a model call: the retrieval future is polled once inline. With a local
//! embedder and no rerank due it finishes on that poll and the snapshot answers the current
//! prompt; otherwise it continues on the runtime and is injected at the next step (one turn of
//! lag), unless it went stale first. The same job then checks the extraction triggers and runs
//! maintenance, queueing their log entries on the memory's outbox.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Waker};

use aho_cap::ContextFn;
use aho_embed::{Vector, similarity};
use aho_log::{Item, State};
use aho_memory::{Entry, Suppression, Suppressor, Trigger, error_event, skill_suggestions};
use aho_types::{BoxFuture, ContentBlock, EntryId, Form, Message, Role, Timestamp};

use crate::llm::clip;
use crate::local::{LocalMemory, TOPIC_COSINE};

/// A topic change extracts only if the previous topic lasted at least this many user turns.
pub const TOPIC_MIN_TURNS: u32 = 4;
const QUERY_CHARS: usize = 2000;

/// Counts user turns since the last extraction and fires the extraction triggers.
#[derive(Clone, Debug)]
pub struct TopicTracker {
    last: Option<Vector>,
    turns: u32,
    every: u32,
}

impl TopicTracker {
    /// `every = 0` disables the periodic trigger.
    pub fn new(every: u32) -> TopicTracker {
        TopicTracker { last: None, turns: 0, every }
    }

    /// Call once per fresh user turn with that turn's embedding (`None` if embedding failed:
    /// the turn still counts, the topic cannot be compared).
    pub fn observe(&mut self, v: Option<&Vector>) -> Option<Trigger> {
        let changed = match (&self.last, v) {
            (Some(a), Some(b)) => similarity(a, b).is_some_and(|s| s < TOPIC_COSINE),
            _ => false,
        };
        if let Some(v) = v {
            self.last = Some(v.clone());
        }
        if changed && self.turns >= TOPIC_MIN_TURNS {
            // this turn opens the new topic
            self.turns = 1;
            return Some(Trigger::TopicChange);
        }
        self.turns += 1;
        if self.every > 0 && self.turns >= self.every {
            self.turns = 0;
            return Some(Trigger::EveryNTurns(self.every));
        }
        None
    }

    pub fn reset(&mut self) {
        self.turns = 0;
    }
}

/// One fresh user turn to process.
pub struct Job {
    pub user: EntryId,
    pub query: String,
    /// Log items after the extraction watermark, as of the turn's first step.
    pub items: Vec<(EntryId, Item)>,
}

struct Pending {
    user: EntryId,
    computed_at: Timestamp,
    entries: Vec<Entry>,
}

struct ProducerState {
    last_user: Option<EntryId>,
    pending: Option<Pending>,
    suppressor: Suppressor,
    topic: TopicTracker,
    /// Items up to here were already offered to the extractors.
    watermark: Option<EntryId>,
    /// Items since the watermark as of the latest fresh turn (the session-end window).
    tail: Vec<(EntryId, Item)>,
}

pub struct Producer {
    mem: Arc<LocalMemory>,
    st: Mutex<ProducerState>,
    jobs: tokio::sync::Mutex<()>,
}

fn to_messages(items: &[(EntryId, Item)]) -> Vec<Message> {
    items
        .iter()
        .filter_map(|(_, it)| match it {
            // Snapshots/notices are injected context (possibly our own), never extraction input.
            Item::User { form: Form::Prompt, text } => Some(Message::user(text.clone())),
            Item::Assistant { content } => Some(Message::assistant(content.clone())),
            Item::Result { call, content, is_error, .. } => Some(Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult { id: call.clone(), content: content.clone(), is_error: *is_error }],
            }),
            _ => None,
        })
        .collect()
}

/// The Snapshot text for `ids` (rank order).
pub fn render(ids: &[u64], by_id: &BTreeMap<u64, Entry>) -> String {
    let entries: Vec<&Entry> = ids.iter().filter_map(|id| by_id.get(id)).collect();
    let (skills, facts): (Vec<&Entry>, Vec<&Entry>) = entries.into_iter().partition(|e| e.skill.is_some());
    let mut s = String::new();
    if !facts.is_empty() {
        s.push_str("Recalled from long-term memory (saved in earlier sessions; may be outdated, so verify before relying on it):\n");
        for e in facts {
            let tags = if e.tags.is_empty() { String::new() } else { format!(" [{}]", e.tags.join(", ")) };
            let _ = writeln!(s, "- #{}{tags}: {}", e.id, e.text);
        }
    }
    if !skills.is_empty() {
        s.push_str("Skills that match this request:\n");
        for e in skills {
            let _ = writeln!(s, "- {}", e.text);
        }
    }
    s.trim_end().to_string()
}

impl Producer {
    pub fn new(mem: Arc<LocalMemory>, suppression: Suppression, extract_every: u32) -> Arc<Producer> {
        Arc::new(Producer {
            mem,
            st: Mutex::new(ProducerState {
                last_user: None,
                pending: None,
                suppressor: Suppressor::new(suppression),
                topic: TopicTracker::new(extract_every),
                watermark: None,
                tail: Vec::new(),
            }),
            jobs: tokio::sync::Mutex::new(()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, ProducerState> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A job when `state` holds a user prompt newer than the last one seen.
    pub fn observe(&self, state: &State) -> Option<Job> {
        let (id, text) = state.items.iter().rev().find_map(|(id, it)| match it {
            Item::User { form: Form::Prompt, text } => Some((*id, text)),
            _ => None,
        })?;
        let mut st = self.lock();
        if st.last_user.is_some_and(|last| last >= id) {
            return None;
        }
        st.last_user = Some(id);
        let wm = st.watermark;
        let items: Vec<(EntryId, Item)> = state.items.iter().filter(|(eid, _)| wm.is_none_or(|w| *eid > w)).cloned().collect();
        st.tail = items.clone();
        Some(Job { user: id, query: clip(text, QUERY_CHARS), items })
    }

    /// Retrieval (sets the pending snapshot), then extraction triggers, then maintenance.
    pub async fn run(&self, job: Job) {
        let _serial = self.jobs.lock().await;
        let mem = &self.mem;
        let qvec = mem.embed_one(&job.query).await.ok();
        match mem.retrieve_with(&job.query, qvec.clone(), mem.settings().k).await {
            Ok(entries) => {
                mem.emit(skill_suggestions(&entries));
                let mut st = self.lock();
                if st.pending.as_ref().is_none_or(|p| p.user <= job.user) {
                    st.pending = Some(Pending { user: job.user, computed_at: mem.clock().now(), entries });
                }
            }
            Err(e) => mem.emit([error_event("retrieve", &e.to_string())]),
        }

        let planned = {
            let mut st = self.lock();
            let wm = st.watermark;
            st.topic.observe(qvec.as_ref()).map(|t| {
                let window: Vec<(EntryId, Item)> = job
                    .items
                    .iter()
                    .filter(|(id, _)| wm.is_none_or(|w| *id > w) && (t != Trigger::TopicChange || *id < job.user))
                    .cloned()
                    .collect();
                (t, window)
            })
        };
        if let Some((trigger, window)) = planned {
            let upto = window.last().map(|(id, _)| *id);
            let (_, events) = mem.extract_logged(trigger, &to_messages(&window)).await;
            mem.emit(events);
            if let Some(w) = upto {
                let mut st = self.lock();
                if st.watermark.is_none_or(|x| w > x) {
                    st.watermark = Some(w);
                }
            }
        }

        match mem.maintain_now() {
            Ok(events) => mem.emit(events),
            Err(e) => mem.emit([error_event("maintain", &e.to_string())]),
        }
    }

    /// The pending retrieval as Snapshot text, if it passes suppression. Consumes it either way.
    pub fn take_snapshot(&self) -> Option<String> {
        let now = self.mem.clock().now();
        let (kept, text) = {
            let mut st = self.lock();
            let p = st.pending.take()?;
            let ids: Vec<u64> = p.entries.iter().map(|e| e.id).collect();
            let by_id: BTreeMap<u64, Entry> = p.entries.into_iter().map(|e| (e.id, e)).collect();
            st.suppressor.admit(p.computed_at, &ids, |ids| render(ids, &by_id), now).ok()?
        };
        if let Err(e) = self.mem.mark_injected(&kept, now) {
            self.mem.emit([error_event("mark injected", &e.to_string())]);
        }
        Some(text)
    }

    /// The `Registrar::context` body.
    pub fn context(self: &Arc<Self>, state: &State) -> Option<String> {
        if let Some(job) = self.observe(state) {
            let me = self.clone();
            let mut fut: BoxFuture<'static, ()> = Box::pin(async move { me.run(job).await });
            let mut cx = Context::from_waker(Waker::noop());
            if fut.as_mut().poll(&mut cx).is_pending() {
                if let Ok(rt) = tokio::runtime::Handle::try_current() {
                    rt.spawn(fut);
                } else {
                    self.mem.emit([error_event(
                        "retrieve",
                        "no async runtime to finish the retrieval on; this turn gets no recalled memories",
                    )]);
                }
            }
        }
        self.take_snapshot()
    }

    pub fn context_fn(self: &Arc<Self>) -> ContextFn {
        let me = self.clone();
        Arc::new(move |state| me.context(state))
    }

    /// Session end: extract from everything since the watermark.
    pub async fn session_end(&self) {
        let _serial = self.jobs.lock().await;
        let window: Vec<(EntryId, Item)> = {
            let st = self.lock();
            let wm = st.watermark;
            st.tail.iter().filter(|(id, _)| wm.is_none_or(|w| *id > w)).cloned().collect()
        };
        if !window.iter().any(|(_, it)| matches!(it, Item::User { form: Form::Prompt, .. })) {
            return;
        }
        let upto = window.last().map(|(id, _)| *id);
        let (_, events) = self.mem.extract_logged(Trigger::SessionEnd, &to_messages(&window)).await;
        self.mem.emit(events);
        let mut st = self.lock();
        if upto.is_some() {
            st.watermark = upto;
        }
        st.topic.reset();
    }
}
