//! Diagnostics after a write: the `post_execute` that tells the written file's language server
//! about the new content and appends the errors and warnings it reports to the `write` / `edit` /
//! `str_replace_editor` result, plus the late notices for what arrives after that result.
//!
//! Per write, one background task (spawned on the runtime; it outlives the call):
//! 1. `didOpen`/`didChange`, then the server's own diagnostics for that content (pulled where
//!    offered, else the settled push), waiting for indexing if needed;
//! 2. hands them to the waiting `post_execute`, which gives up after `write_diagnostics_wait_ms`
//!    (default under `tools.grace_ms`, so the write's result is not turned into a late one);
//! 3. with `check_on_save`, `didSave`; when the server starts work-done progress right after it
//!    (rust-analyzer's `cargo check`), waits for it to end and merges what it pushed for the file;
//! 4. if the file's errors/warnings now differ from what the write result showed (the check found
//!    more, or step 1 missed the `post_execute` deadline), queues a late notice. The capability's
//!    `context` fn hands queued notices to the session, which logs them at the next request.
//!
//! Steps 1–3 stop at `late_diagnostics_wait_ms` from the write. A newer write of the same file
//! drops the older write's notice. No server for the file, a server that fails to start, no
//! errors or warnings, or a non-multi-thread runtime: the result is unchanged and nothing is queued.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aho_cap::{CancelToken, ExecOutcome, ExecView, PostDecision};
use aho_types::ContentBlock;
use serde_json::Value;
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::sync::oneshot;

use crate::ToolLsp;
use crate::client::Client;
use crate::ops::{diagnostic_line, merge_diagnostics};
use crate::pool::{Pool, Server, Synced, language_id};
use crate::text::{self, Encoding};

/// The tools whose successful results get the written file's diagnostics.
pub const WRITE_TOOLS: &[&str] = &["write", "edit", "str_replace_editor"];
/// Most diagnostics in one write result or late notice.
pub const WRITE_MAX: usize = 20;
/// A pushed list counts as final once nothing newer arrived for this long.
const WRITE_SETTLE: Duration = Duration::from_millis(250);
/// How soon after `didSave` an on-save check must announce itself to be waited for.
const CHECK_START: Duration = Duration::from_millis(500);
/// After the check's progress ends, how long its last publish may trail it.
const CHECK_SETTLE: Duration = Duration::from_millis(300);
/// Floor for one pull request near the deadline.
const MIN_REQUEST: Duration = Duration::from_millis(250);

const TEMPLATES: &str = include_str!("../prompts/write-diagnostics.md");

fn template(key: &str) -> &'static str {
    TEMPLATES.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix(": ")).unwrap_or("")
}

fn left(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn severity(d: &Value) -> u64 {
    d.get("severity").and_then(Value::as_u64).unwrap_or(0)
}

fn start(d: &Value) -> (u64, u64) {
    let at = |k: &str| d.pointer(&format!("/range/start/{k}")).and_then(Value::as_u64).unwrap_or(0);
    (at("line"), at("character"))
}

/// What the server said about one written file.
pub struct WriteReport {
    /// Workspace-relative path.
    pub rel: String,
    /// The content the diagnostics are for (columns are converted against it).
    pub text: String,
    pub encoding: Encoding,
    pub items: Vec<Value>,
}

impl WriteReport {
    /// Errors, then warnings, by position, as `path:line:col severity [code]: message (source)`.
    pub fn rows(&self) -> Vec<String> {
        let mut shown: Vec<&Value> = self.items.iter().filter(|d| matches!(severity(d), 1 | 2)).collect();
        shown.sort_by_key(|d| (severity(d), start(d)));
        shown
            .into_iter()
            .map(|d| {
                let (line, units) = start(d);
                let units = u32::try_from(units).unwrap_or(u32::MAX);
                let column =
                    text::line(&self.text, line as usize).map(|l| text::from_lsp_col(l, units, self.encoding)).unwrap_or(units as usize);
                diagnostic_line(&self.rel, line as usize + 1, column + 1, d)
            })
            .collect()
    }

    /// `header` and at most `WRITE_MAX` rows; `None` when there are no errors or warnings.
    pub fn render(&self, header: &str) -> Option<String> {
        let rows = self.rows();
        if rows.is_empty() {
            return None;
        }
        let mut out = header.to_string();
        for row in rows.iter().take(WRITE_MAX) {
            out.push('\n');
            out.push_str(row);
        }
        if rows.len() > WRITE_MAX {
            out.push('\n');
            out.push_str(&template("more").replace("{n}", &(rows.len() - WRITE_MAX).to_string()));
        }
        Some(out)
    }
}

/// Late notices waiting for the next request, and the newest write per file.
#[derive(Default)]
pub struct Late {
    state: Mutex<LateState>,
}

#[derive(Default)]
struct LateState {
    /// Per file: the generation of its newest write.
    writes: HashMap<String, u64>,
    next: u64,
    queued: BTreeMap<String, WriteReport>,
    /// Notices handed out so far; numbers them so a repeated list is never deduplicated away.
    notices: u64,
}

impl Late {
    /// A new write of `rel`: its older notice is stale. Returns the write's generation.
    fn begin(&self, rel: &str) -> u64 {
        let mut st = lock(&self.state);
        st.next += 1;
        let generation = st.next;
        st.writes.insert(rel.to_string(), generation);
        st.queued.remove(rel);
        generation
    }

    /// Queues `report` unless a newer write of its file started since.
    fn offer(&self, generation: u64, report: WriteReport) {
        let mut st = lock(&self.state);
        if st.writes.get(&report.rel) == Some(&generation) {
            st.queued.insert(report.rel.clone(), report);
        }
    }

    /// The queued notices as one text (each file's current errors and warnings), emptying the
    /// queue. The capability's `context` fn.
    pub fn drain(&self) -> Option<String> {
        let mut st = lock(&self.state);
        let queued = std::mem::take(&mut st.queued);
        let n = (st.notices + 1).to_string();
        let blocks: Vec<String> =
            queued.values().filter_map(|r| r.render(&template("late").replace("{path}", &r.rel).replace("{n}", &n))).collect();
        if blocks.is_empty() {
            return None;
        }
        st.notices += 1;
        Some(blocks.join("\n\n"))
    }
}

/// Between a write's `post_execute` and its background task: whoever takes `report` delivers it.
struct Handoff {
    report: Arc<Mutex<Option<WriteReport>>>,
    /// Dropped by the task once `report` is filled or it gave up: wakes `post_execute`.
    ready: oneshot::Sender<()>,
    /// Closes when `post_execute` took `report` or stopped waiting (it drops the sender).
    decided: oneshot::Receiver<()>,
    late: Arc<Late>,
    generation: u64,
}

impl ToolLsp {
    /// The registered `post_execute`: a successful write/edit of a file some configured server
    /// covers gets that file's diagnostics appended; everything else passes through unchanged.
    /// Blocks the calling worker (`block_in_place`) for at most `write_diagnostics_wait_ms`.
    pub fn after_write(&self, view: &ExecView, outcome: &ExecOutcome) -> PostDecision {
        let unchanged = || PostDecision::Accept { content: None, deferred_context: Vec::new() };
        let cfg = &self.pool.cfg;
        if !cfg.diagnostics_on_write || !WRITE_TOOLS.contains(&view.tool.as_str()) {
            return unchanged();
        }
        let ExecOutcome::Done { value, content, .. } = outcome else { return unchanged() };
        // `str_replace_editor view` and a write that changed nothing carry no new content.
        let wrote = value.get("created").and_then(Value::as_bool) == Some(true)
            || value.get("hunks").and_then(Value::as_array).is_some_and(|h| !h.is_empty());
        let Some(path) = value.get("path").and_then(Value::as_str).filter(|_| wrote) else { return unchanged() };
        let Ok(rel) = self.pool.rel(path) else { return unchanged() };
        if cfg.for_path(&rel).is_none() {
            return unchanged();
        }
        let Ok(rt) = Handle::try_current() else { return unchanged() };
        if rt.runtime_flavor() != RuntimeFlavor::MultiThread {
            return unchanged();
        }
        let slot = Arc::new(Mutex::new(None));
        let (ready, ready_rx) = oneshot::channel();
        let (decided_tx, decided) = oneshot::channel();
        let generation = self.late.begin(&rel);
        let handoff = Handoff { report: slot.clone(), ready, decided, late: self.late.clone(), generation };
        rt.spawn(self.pool.clone().write_diagnostics(rel, handoff));
        let wait = Duration::from_millis(cfg.write_wait_ms);
        // Ready, gave up, or out of time: what is in the slot now is this result's to show.
        drop(tokio::task::block_in_place(|| rt.block_on(tokio::time::timeout(wait, ready_rx))));
        let report = lock(&slot).take();
        drop(decided_tx);
        let Some(block) = report.and_then(|r| r.render(&template("header").replace("{path}", &r.rel))) else { return unchanged() };
        let mut content = content.clone();
        content.push(ContentBlock::text(block));
        PostDecision::Accept { content: Some(content), deferred_context: Vec::new() }
    }

    /// Queued late notices (see `Late::drain`); registered as the capability's `context`.
    pub fn late_notice(&self) -> Option<String> {
        self.late.drain()
    }
}

impl Pool {
    /// One write's background task (module docs, steps 1–4). Returns early, queueing nothing,
    /// when there is no server or it fails.
    async fn write_diagnostics(self: Arc<Self>, rel: String, h: Handoff) {
        let Some(own) = self.own_diagnostics(&rel).await else { return };
        let (server, synced, native) = own;
        let report = |items: Vec<Value>| WriteReport { rel: rel.clone(), text: synced.text.clone(), encoding: server.encoding, items };
        *lock(&h.report) = Some(report(native.clone()));
        drop(h.ready);
        let deadline = synced.content_at + Duration::from_millis(self.cfg.late_wait_ms);
        let mut items = native.clone();
        if self.cfg.check_on_save {
            let Ok(saved) = server.save(&synced).await else { return };
            if let Some(saved) = saved {
                check(&server.client, &synced, saved, deadline).await;
                if let Some(p) = server.client.diagnostics(&synced.uri).filter(|p| p.at >= saved) {
                    merge_diagnostics(&mut items, p.items);
                }
            }
        }
        // Whatever `post_execute` did not take was never shown.
        drop(h.decided.await);
        let shown = if lock(&h.report).take().is_some() { Vec::new() } else { native };
        let now = report(items);
        if now.rows() != report(shown).rows() {
            h.late.offer(h.generation, now);
        }
    }

    /// Server, synced document and the server's own diagnostics for it (empty when it was still
    /// indexing at `late_diagnostics_wait_ms`).
    async fn own_diagnostics(&self, rel: &str) -> Option<(Arc<Server>, Synced, Vec<Value>)> {
        let cfg = self.cfg.for_path(rel)?;
        let cancel = CancelToken::new();
        let root = self.root_for(cfg, rel).await;
        let server = self.server(cfg, &root, &cancel).await.ok()?;
        let synced = server.sync(self.fs.as_ref(), rel, &language_id(&cfg.language, rel)).await.ok()?;
        let deadline = synced.content_at + Duration::from_millis(self.cfg.late_wait_ms);
        let items = if !server.client.wait_ready(left(deadline), &cancel).await.ok()? {
            Vec::new()
        } else if server.supports("diagnosticProvider") {
            let wait = left(deadline).min(self.request_timeout());
            self.pull_diagnostics(&server, &synced.uri, wait, wait.max(MIN_REQUEST), &cancel).await.unwrap_or_default()
        } else {
            pushed(&server, &synced, deadline, &cancel).await?
        };
        Some((server, synced, items))
    }
}

/// Waits for an on-save check: progress beginning within `CHECK_START` of `saved`, then its end
/// and the publish trailing it, all before `deadline`. Returns at once when no check started.
async fn check(client: &Client, synced: &Synced, saved: Instant, deadline: Instant) {
    let cancel = CancelToken::new();
    let began = |c: &Client| c.progress_began().is_some_and(|t| t >= saved);
    if !matches!(client.wait_until(left((saved + CHECK_START).min(deadline)), &cancel, began).await, Ok(true)) {
        return;
    }
    if !matches!(client.wait_until(left(deadline), &cancel, |c| !c.busy()).await, Ok(true)) {
        return;
    }
    let ended = Instant::now();
    let trailing = |c: &Client| c.diagnostics(&synced.uri).is_some_and(|p| p.at >= ended);
    drop(client.wait_until(left((ended + CHECK_SETTLE).min(deadline)), &cancel, trailing).await);
}

/// The server's push for exactly this content once it settled (or whatever current list exists
/// at the deadline); empty when none came.
async fn pushed(server: &Server, synced: &Synced, deadline: Instant, cancel: &CancelToken) -> Option<Vec<Value>> {
    let current = |p: &crate::client::Published| p.at >= synced.content_at && p.version.is_none_or(|v| v >= synced.version);
    let settled = |c: &Client| c.diagnostics(&synced.uri).is_some_and(|p| current(&p) && p.at.elapsed() >= WRITE_SETTLE);
    server.client.wait_until(left(deadline), cancel, settled).await.ok()?;
    Some(server.client.diagnostics(&synced.uri).filter(|p| current(p)).map(|p| p.items).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn diag(line: u64, character: u64, severity: u64, message: &str) -> Value {
        json!({"range": {"start": {"line": line, "character": character}, "end": {"line": line, "character": character + 1}},
               "severity": severity, "message": message})
    }

    fn report(rel: &str, items: Vec<Value>) -> WriteReport {
        WriteReport { rel: rel.into(), text: "fn a() {}\nlet 😀 = x;\n".into(), encoding: Encoding::Utf16, items }
    }

    #[test]
    fn errors_come_before_warnings_infos_are_dropped_and_columns_count_characters() {
        let r = report(
            "src/a.rs",
            vec![diag(0, 3, 2, "unused"), diag(1, 9, 1, "no `x`\nin scope"), diag(0, 0, 3, "info"), diag(0, 1, 1, "first")],
        );
        assert_eq!(r.rows(), ["src/a.rs:1:2 error: first", "src/a.rs:2:9 error: no `x` in scope", "src/a.rs:1:4 warning: unused"]);
        assert!(report("src/a.rs", vec![diag(0, 0, 3, "info"), diag(0, 0, 4, "hint")]).render("h").is_none());
    }

    #[test]
    fn the_list_is_capped_with_the_remainder_counted() {
        let r = report("src/a.rs", (0..WRITE_MAX as u64 + 3).map(|l| diag(l, 0, 1, "e")).collect()).render("h").unwrap();
        assert_eq!(r.lines().filter(|l| l.starts_with("src/a.rs:")).count(), WRITE_MAX);
        assert!(r.lines().last().unwrap().contains(" 3 more"), "{r}");
    }

    #[test]
    fn late_notices_drop_stale_writes_drain_once_and_never_repeat_the_same_text() {
        let late = Late::default();
        let old = late.begin("a.rs");
        let new = late.begin("a.rs");
        late.offer(old, report("a.rs", vec![diag(0, 0, 1, "stale")]));
        assert!(late.drain().is_none(), "an older write's findings are dropped");
        late.offer(new, report("a.rs", vec![diag(0, 0, 1, "borrow")]));
        let b = late.begin("b.rs");
        late.offer(b, report("b.rs", vec![diag(1, 0, 2, "unused")]));
        let first = late.drain().unwrap();
        assert!(first.contains("a.rs:1:1 error: borrow") && first.contains("b.rs:2:1 warning: unused"), "{first}");
        assert!(late.drain().is_none(), "drained once");
        let again = late.begin("a.rs");
        late.offer(again, report("a.rs", vec![diag(0, 0, 1, "borrow")]));
        let second = late.drain().unwrap();
        assert_ne!(first, second, "a repeated list still reaches the log");
        // A queued notice is replaced by the next write of the file.
        let w = late.begin("c.rs");
        late.offer(w, report("c.rs", vec![diag(0, 0, 1, "x")]));
        late.begin("c.rs");
        assert!(late.drain().is_none());
    }
}
