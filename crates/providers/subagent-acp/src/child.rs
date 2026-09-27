//! One external ACP agent as seen from aho, the ACP client: its `session/update`s are mirrored
//! into the parent log as `Subagent/*` entries, and its `session/request_permission` requests
//! are decided by the parent's operator through the parent's Approval handle: never by the
//! agent, and never wider than the child's policy.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use aho_acp::schema::{
    ContentBlock, PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SessionNotification, SessionUpdate, ToolCallContent, ToolCallStatus, ToolKind,
};
use aho_acp::{Handler, HandlerFuture, RpcError, methods};
use aho_approval::{Approval, Decided};
use aho_log::{ApprovalOutcome, EntryKind, GrantScope, LogStore};
use aho_policy::{ApprovalCard, DecisionReason, SandboxPolicy};
use aho_types::{Consequence, Principal, SessionId, Value};
use serde_json::json;
use tokio::sync::watch;

use crate::PROVIDER;

/// Stderr lines of the agent kept for diagnostics.
pub const STDERR_LINES: usize = 40;

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The consequence class an agent's tool kind claims. The claim comes from the agent, so it is
/// only ever used to refuse (narrowing); what the operator is asked is decided by the operator.
pub fn class_of(kind: ToolKind) -> Option<Consequence> {
    match kind {
        ToolKind::Read | ToolKind::Search | ToolKind::Think => Some(Consequence::ReadOnly),
        ToolKind::Edit | ToolKind::Delete | ToolKind::Move => Some(Consequence::WorkspaceWrite),
        ToolKind::Execute => Some(Consequence::SpawnsProcess),
        ToolKind::Fetch => Some(Consequence::NetworkEgress),
        ToolKind::SwitchMode | ToolKind::Other => None,
    }
}

fn kind_name(k: ToolKind) -> String {
    serde_json::to_value(k).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "other".into())
}

#[derive(Clone, Default)]
struct ToolSeen {
    title: Option<String>,
    name: Option<String>,
    kind: Option<ToolKind>,
    done: bool,
}

#[derive(Default)]
struct Acc {
    message_id: Option<String>,
    text: String,
    thought: String,
    last_message: Option<String>,
    sent: u64,
}

/// Mirror sink: `Custom{ns: "Subagent"}` entries in the parent log, by `System("subagent-acp")`,
/// every body carrying `{child, provider}` (the convention the foreign-CLI providers share).
pub(crate) struct Mirror {
    log: Arc<dyn LogStore>,
    child: SessionId,
    failures: Mutex<Vec<String>>,
}

impl Mirror {
    pub(crate) fn new(log: Arc<dyn LogStore>, child: SessionId) -> Mirror {
        Mirror { log, child, failures: Mutex::new(Vec::new()) }
    }

    pub(crate) fn put(&self, name: &str, mut body: Value) {
        if let Value::Object(m) = &mut body {
            m.insert("child".into(), Value::String(self.child.0.to_string()));
            m.insert("provider".into(), Value::String(PROVIDER.into()));
        }
        let kind = EntryKind::Custom { ns: "Subagent".into(), name: name.into(), body };
        if let Err(e) = self.log.append(None, Principal::system(PROVIDER), kind) {
            lock(&self.failures).push(format!("Subagent/{name} was not recorded in the parent log ({e})"));
        }
    }

    /// Mirror writes that failed since the last call (they go into the run's diagnostic).
    pub(crate) fn take_failures(&self) -> Vec<String> {
        std::mem::take(&mut *lock(&self.failures))
    }
}

/// Longest a failure report waits for the agent's stderr to close.
pub const STDERR_SETTLE: std::time::Duration = std::time::Duration::from_secs(2);

/// The tail of an agent's stderr. A child with no stderr (an in-process agent) starts closed.
pub(crate) struct Stderr {
    lines: Mutex<VecDeque<String>>,
    closed: watch::Sender<bool>,
}

impl Stderr {
    fn new() -> Stderr {
        Stderr { lines: Mutex::new(VecDeque::new()), closed: watch::channel(false).0 }
    }
    pub(crate) fn push(&self, line: String) {
        let mut l = lock(&self.lines);
        if l.len() == STDERR_LINES {
            l.pop_front();
        }
        l.push_back(line);
    }
    pub(crate) fn close(&self) {
        self.closed.send_replace(true);
    }
    /// Whether stderr closed within [`STDERR_SETTLE`]; timing out only shortens the report.
    async fn settled(&self) -> bool {
        let mut rx = self.closed.subscribe();
        matches!(tokio::time::timeout(STDERR_SETTLE, rx.wait_for(|c| *c)).await, Ok(Ok(_)))
    }
    fn tail(&self) -> Option<String> {
        let s = lock(&self.lines);
        (!s.is_empty()).then(|| s.iter().cloned().collect::<Vec<_>>().join("\n"))
    }
}

/// State the connection's handler and the provider share for one child.
pub(crate) struct ChildShared {
    pub(crate) label: String,
    pub(crate) agent: String,
    pub(crate) policy: SandboxPolicy,
    pub(crate) approval: Arc<dyn Approval>,
    pub(crate) mirror: Mirror,
    pub(crate) session: Mutex<Option<String>>,
    pub(crate) stderr: Arc<Stderr>,
    /// Bumped on every interrupt: permission requests still waiting answer `cancelled`.
    pub(crate) cancel: watch::Sender<u64>,
    acc: Mutex<Acc>,
    tools: Mutex<HashMap<String, ToolSeen>>,
}

impl ChildShared {
    pub(crate) fn new(label: String, agent: String, policy: SandboxPolicy, approval: Arc<dyn Approval>, mirror: Mirror) -> ChildShared {
        ChildShared {
            label,
            agent,
            policy,
            approval,
            mirror,
            session: Mutex::new(None),
            stderr: Arc::new(Stderr::new()),
            cancel: watch::channel(0).0,
            acc: Mutex::new(Acc::default()),
            tools: Mutex::new(HashMap::new()),
        }
    }

    /// The agent's last stderr lines, once its stderr has closed (bounded wait: an agent that
    /// exited closes it at once; one that hangs gets [`STDERR_SETTLE`]).
    pub(crate) async fn stderr_tail(&self) -> Option<String> {
        self.stderr.settled().await;
        self.stderr.tail()
    }

    /// A run starts: `Subagent/Sent`, fresh accumulators.
    pub(crate) fn begin_run(&self, text: &str) {
        let seq = {
            let mut a = lock(&self.acc);
            *a = Acc { sent: a.sent + 1, ..Acc::default() };
            a.sent
        };
        self.mirror.put("Sent", json!({ "seq": seq, "starts_run": true, "text": text }));
    }

    /// A run ended: flushes what is still accumulating and returns the last agent message.
    pub(crate) fn end_run(&self) -> Option<String> {
        self.flush();
        lock(&self.acc).last_message.clone()
    }

    fn flush(&self) {
        let (text, thought) = {
            let mut a = lock(&self.acc);
            let text = std::mem::take(&mut a.text);
            let thought = std::mem::take(&mut a.thought);
            if !text.trim().is_empty() {
                a.last_message = Some(text.trim().to_string());
            }
            (text, thought)
        };
        if !thought.trim().is_empty() {
            self.mirror.put("Note", json!({ "text": thought, "thinking": true }));
        }
        if !text.trim().is_empty() {
            self.mirror.put("Message", json!({ "text": text }));
        }
    }

    fn chunk(&self, content: &ContentBlock, message_id: &Option<String>, thought: bool) {
        let ContentBlock::Text(t) = content else { return };
        let new_message = {
            let a = lock(&self.acc);
            message_id.is_some() && a.message_id.is_some() && a.message_id != *message_id
        };
        if new_message {
            self.flush();
        }
        let mut a = lock(&self.acc);
        if message_id.is_some() {
            a.message_id = message_id.clone();
        }
        if thought { a.thought.push_str(&t.text) } else { a.text.push_str(&t.text) }
    }

    pub(crate) fn update(&self, n: SessionNotification) {
        if lock(&self.session).as_deref().is_some_and(|s| s != n.session_id) {
            return;
        }
        match n.update {
            SessionUpdate::AgentMessageChunk(c) => self.chunk(&c.content, &c.message_id, false),
            SessionUpdate::AgentThoughtChunk(c) => self.chunk(&c.content, &c.message_id, true),
            SessionUpdate::ToolCall(tc) => {
                self.flush();
                lock(&self.tools).insert(
                    tc.tool_call_id.clone(),
                    ToolSeen { title: Some(tc.title.clone()), name: tc.name.clone(), kind: Some(tc.kind), done: false },
                );
                self.mirror.put(
                    "ToolUse",
                    json!({ "id": tc.tool_call_id, "name": tc.name.clone().unwrap_or(tc.title.clone()), "title": tc.title, "kind": kind_name(tc.kind), "input": tc.raw_input }),
                );
                if matches!(tc.status, ToolCallStatus::Completed | ToolCallStatus::Failed) {
                    self.tool_done(&tc.tool_call_id, tc.status, &tc.content, &tc.raw_output);
                }
            }
            SessionUpdate::ToolCallUpdate(u) => {
                {
                    let mut tools = lock(&self.tools);
                    let seen = tools.entry(u.tool_call_id.clone()).or_default();
                    if u.title.is_some() {
                        seen.title = u.title.clone();
                    }
                    if u.name.is_some() {
                        seen.name = u.name.clone();
                    }
                    if u.kind.is_some() {
                        seen.kind = u.kind;
                    }
                }
                if let Some(s @ (ToolCallStatus::Completed | ToolCallStatus::Failed)) = u.status {
                    self.tool_done(&u.tool_call_id, s, u.content.as_deref().unwrap_or(&[]), &u.raw_output);
                }
            }
            SessionUpdate::Plan(p) => {
                let entries: Vec<Value> =
                    p.entries.iter().map(|e| json!({ "content": e.content, "status": e.status, "priority": e.priority })).collect();
                self.mirror.put("Note", json!({ "plan": entries }));
            }
            _ => {}
        }
    }

    fn tool_done(&self, id: &str, status: ToolCallStatus, content: &[ToolCallContent], raw: &Option<Value>) {
        {
            let mut tools = lock(&self.tools);
            let seen = tools.entry(id.to_string()).or_default();
            if seen.done {
                return;
            }
            seen.done = true;
        }
        let text: Vec<String> = content
            .iter()
            .filter_map(|c| match c {
                ToolCallContent::Content { content: ContentBlock::Text(t) } => Some(t.text.clone()),
                ToolCallContent::Diff { path, .. } => Some(format!("[diff of {path}]")),
                ToolCallContent::Terminal { terminal_id } => Some(format!("[terminal {terminal_id}]")),
                _ => None,
            })
            .collect();
        self.mirror.put(
            "ToolResult",
            json!({ "id": id, "is_error": status == ToolCallStatus::Failed, "status": status, "content": text.join("\n"), "output": raw }),
        );
    }

    /// Decides one `session/request_permission`. Refused outright when the claimed kind is
    /// outside the child's policy; otherwise the parent's operator decides through the Approval
    /// handle (an async, unbubbled child has none: refused). An interrupt answers `cancelled`.
    pub(crate) async fn permission(self: Arc<Self>, params: Value) -> Result<Value, RpcError> {
        let req: RequestPermissionRequest = aho_acp::params(methods::SESSION_REQUEST_PERMISSION, params)?;
        let mut cancelled = self.cancel.subscribe();
        cancelled.mark_unchanged();
        let tc = &req.tool_call;
        let seen = lock(&self.tools).get(&tc.tool_call_id).cloned().unwrap_or_default();
        let kind = tc.kind.or(seen.kind).unwrap_or_default();
        let title = tc.title.clone().or(seen.title).unwrap_or_else(|| tc.tool_call_id.clone());
        let name = tc.name.clone().or(seen.name).unwrap_or_else(|| title.clone());
        let id = tc.tool_call_id.clone();

        let outcome = match class_of(kind) {
            Some(c) if !self.policy.allowed().contains(c) => {
                let reason = format!(
                    "`{title}` ({}) is {} work, outside this subagent's policy; it was refused without asking the operator",
                    kind_name(kind),
                    c.name()
                );
                self.mirror.put("Blocked", json!({ "id": id, "reason": reason }));
                pick(&req.options, &[PermissionOptionKind::RejectOnce, PermissionOptionKind::RejectAlways])
            }
            claimed => {
                let class = claimed.unwrap_or(Consequence::SpawnsProcess);
                let what = match claimed {
                    Some(_) => format!("({})", kind_name(kind)),
                    None => format!("(kind `{}`: aho cannot classify it, so it is shown as a process effect)", kind_name(kind)),
                };
                let card = ApprovalCard {
                    tool: format!("acp:{}/{name}", self.agent),
                    class,
                    args: tc.raw_input.clone().unwrap_or_else(|| json!({ "title": title, "kind": kind_name(kind) })),
                    reason: DecisionReason::Delegation,
                    world: self.policy.world().clone(),
                    suggested_cut: None,
                    rendered: format!(
                        "Subagent `{}` ({}, an external ACP agent) asks to run `{title}` {what}. Its own process runs it once allowed; aho cannot enforce policy inside that process.",
                        self.label, self.agent
                    ),
                };
                let decided = tokio::select! {
                    d = self.approval.request(card) => Some(d),
                    _ = cancelled.changed() => None,
                };
                match decided {
                    None => Some(RequestPermissionOutcome::Cancelled),
                    Some(d) => {
                        if d.outcome != ApprovalOutcome::AllowedOnce {
                            let reason = match d.outcome {
                                ApprovalOutcome::Rejected => "the operator rejected it",
                                ApprovalOutcome::Cancelled => "the approval was cancelled before anyone answered",
                                _ => "no operator can answer for this subagent (no approval channel)",
                            };
                            self.mirror.put("Blocked", json!({ "id": id, "reason": reason }));
                        }
                        pick(&req.options, &wanted(d))
                    }
                }
            }
        };
        let outcome = outcome.unwrap_or(RequestPermissionOutcome::Cancelled);
        aho_acp::result(&RequestPermissionResponse { outcome, meta: None })
    }
}

/// The option kinds that express a decision, best first.
fn wanted(d: Decided) -> Vec<PermissionOptionKind> {
    use PermissionOptionKind::*;
    match (d.outcome, d.scope) {
        (ApprovalOutcome::AllowedOnce, GrantScope::Once) => vec![AllowOnce, AllowAlways],
        (ApprovalOutcome::AllowedOnce, _) => vec![AllowAlways, AllowOnce],
        _ => vec![RejectOnce, RejectAlways],
    }
}

/// The first offered option of the wanted kinds; none offered is `None` (answered `cancelled`,
/// which never allows anything).
fn pick(options: &[PermissionOption], kinds: &[PermissionOptionKind]) -> Option<RequestPermissionOutcome> {
    kinds
        .iter()
        .find_map(|k| options.iter().find(|o| o.kind == *k))
        .map(|o| RequestPermissionOutcome::Selected { option_id: o.option_id.clone() })
}

/// The client side of the connection to one agent.
pub(crate) struct ClientSide(pub(crate) Arc<ChildShared>);

impl Handler for ClientSide {
    fn request(&self, method: String, params: Value) -> HandlerFuture {
        match method.as_str() {
            methods::SESSION_REQUEST_PERMISSION => Box::pin(self.0.clone().permission(params)),
            // aho advertised no fs/terminal/elicitation capability; anything else is unknown.
            other => {
                let e = RpcError::method_not_found(other);
                Box::pin(async move { Err(e) })
            }
        }
    }

    fn notification(&self, method: String, params: Value) {
        if method != methods::SESSION_UPDATE {
            return;
        }
        match aho_acp::params::<SessionNotification>(&method, params) {
            Ok(n) => self.0.update(n),
            Err(e) => self
                .0
                .mirror
                .put("Error", json!({ "message": format!("an update from the agent was not understood: {e}"), "retrying": false })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aho_acp::schema::PermissionOptionKind as K;

    fn opts(kinds: &[K]) -> Vec<PermissionOption> {
        kinds
            .iter()
            .enumerate()
            .map(|(i, k)| PermissionOption { option_id: format!("o{i}"), name: "x".into(), kind: *k, meta: None })
            .collect()
    }

    #[test]
    fn decisions_pick_the_closest_offered_option_and_never_an_allow_for_a_refusal() {
        let all = opts(&[K::RejectAlways, K::AllowAlways, K::AllowOnce, K::RejectOnce]);
        let sel = |o: Option<RequestPermissionOutcome>| match o {
            Some(RequestPermissionOutcome::Selected { option_id }) => option_id,
            other => format!("{other:?}"),
        };
        assert_eq!(sel(pick(&all, &wanted(Decided::allowed(GrantScope::Once)))), "o2");
        assert_eq!(sel(pick(&all, &wanted(Decided::allowed(GrantScope::Session)))), "o1");
        assert_eq!(sel(pick(&all, &wanted(Decided::rejected()))), "o3");
        assert_eq!(sel(pick(&all, &wanted(Decided::UNAVAILABLE))), "o3");
        // only allows offered: a refusal cannot be expressed, so nothing is selected (→ cancelled)
        assert_eq!(pick(&opts(&[K::AllowOnce]), &wanted(Decided::rejected())), None);
        // allow-always only: a one-time allow falls back to it
        assert_eq!(sel(pick(&opts(&[K::AllowAlways, K::RejectOnce]), &wanted(Decided::allowed(GrantScope::Once)))), "o0");
    }

    #[test]
    fn claimed_kinds_map_to_classes_and_unclassifiable_ones_to_none() {
        assert_eq!(class_of(ToolKind::Execute), Some(Consequence::SpawnsProcess));
        assert_eq!(class_of(ToolKind::Delete), Some(Consequence::WorkspaceWrite));
        assert_eq!(class_of(ToolKind::Fetch), Some(Consequence::NetworkEgress));
        assert_eq!(class_of(ToolKind::Search), Some(Consequence::ReadOnly));
        assert_eq!(class_of(ToolKind::SwitchMode), None);
        assert_eq!(class_of(ToolKind::Other), None);
    }
}
