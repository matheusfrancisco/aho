//! One ACP session bound to one aho [`Session`]: a forwarder task turns session events and
//! approval cards into `session/update` notifications and `session/request_permission` requests,
//! in log order; `session/prompt` runs a turn and answers once that turn's updates are out.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use aho_acp::schema::{
    PermissionOption, PermissionOptionKind, PromptResponse, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SessionNotification, SessionUpdate, ToolCallContent, ToolCallStatus, ToolCallUpdate,
};
use aho_acp::{Peer, RpcError, methods};
use aho_approval::{ApprovalRequest, Decided};
use aho_log::{Entry, EntryKind, GrantScope};
use aho_session::{Session, SessionError, SessionEvent};
use aho_types::{CallId, EntryId, StopReason as AhoStop};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::Diag;
use crate::map::{self, Mapper};

pub const ALLOW_ONCE: &str = "allow-once";
pub const ALLOW_SESSION: &str = "allow-always";
pub const REJECT_ONCE: &str = "reject-once";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Default)]
struct Turn {
    cancelled: bool,
}

pub(crate) struct Live {
    pub(crate) id: String,
    pub(crate) session: Arc<Session>,
    peer: Peer,
    diag: Diag,
    turn: Mutex<Option<Turn>>,
    /// Highest turn whose `Turn/End` (and everything before it) has been sent.
    turn_end: watch::Sender<u64>,
    stop: watch::Sender<bool>,
}

impl Live {
    /// Binds `session` to ACP session `id` and starts forwarding. With `replay` (session/load) the
    /// head branch is sent as updates first, before this returns, so it precedes the load result.
    pub(crate) fn start(
        id: String,
        session: Arc<Session>,
        peer: Peer,
        diag: Diag,
        approvals: Option<mpsc::Receiver<ApprovalRequest>>,
        replay: bool,
    ) -> Result<Arc<Live>, String> {
        let events = session.subscribe();
        let turns = session.fold().map_err(|e| format!("the session log could not be read: {e}"))?.turns;
        let live = Arc::new(Live {
            id,
            session: session.clone(),
            peer,
            diag,
            turn: Mutex::new(None),
            turn_end: watch::channel(turns).0,
            stop: watch::channel(false).0,
        });
        let mut mapper = Mapper::new(session.policy_view().workspace);
        let mut last = None;
        if replay {
            let entries = match session.log().head() {
                Some(h) => session.log().read_branch(h).map_err(|e| format!("the session log could not be read: {e}"))?,
                None => Vec::new(),
            };
            for e in &entries {
                for u in mapper.entry(e, true) {
                    live.send(u);
                }
            }
            last = entries.last().map(|e| e.id);
        }
        tokio::spawn(forward(live.clone(), events, approvals, mapper, last));
        Ok(live)
    }

    pub(crate) fn stop(&self) {
        self.stop.send_replace(true);
        self.cancel();
    }

    fn send(&self, update: SessionUpdate) {
        let n = SessionNotification { session_id: self.id.clone(), update, meta: None };
        if let Err(e) = self.peer.notify_typed(methods::SESSION_UPDATE, &n) {
            (self.diag)(&format!("session {}: an update was not delivered ({e})", self.id));
        }
    }

    fn entry(&self, m: &mut Mapper, e: &Entry, replay: bool) {
        for u in m.entry(e, replay) {
            self.send(u);
        }
        if let EntryKind::TurnEnd { turn } = &e.kind {
            self.turn_end.send_replace(turn.0);
        }
    }

    fn on_event(&self, m: &mut Mapper, ev: SessionEvent, last: &mut Option<EntryId>) {
        match ev {
            SessionEvent::Entry(e) => {
                if last.is_some_and(|l| e.id <= l) {
                    return;
                }
                *last = Some(e.id);
                self.entry(m, &e, false);
            }
            SessionEvent::Stream(s) => {
                for u in m.stream(&s) {
                    self.send(u);
                }
            }
            SessionEvent::Idle { .. } => {}
            SessionEvent::Error(msg) => (self.diag)(&format!("session {}: {msg}", self.id)),
        }
    }

    /// The broadcast dropped events: the durable part is re-read from the log. Stream chunks that
    /// were lost are covered by the `Assistant/Message` they belong to.
    fn recover(&self, m: &mut Mapper, last: &mut Option<EntryId>) {
        (self.diag)(&format!("session {}: the update stream fell behind; resending from the log", self.id));
        for e in self.session.log().read_from(*last) {
            *last = Some(e.id);
            self.entry(m, &e, false);
        }
    }

    /// The call a card is about: the pending `Approval/Requested` for that tool, else its open
    /// call (a tool asking mid-run), else a synthetic tool call announced first.
    fn card_call(&self, m: &mut Mapper, tool: &str) -> (String, bool) {
        let Ok(state) = self.session.fold() else { return self.synthetic(m) };
        let tool_of = |c: &CallId| state.calls.get(c).map(|(t, _, _)| t.as_str());
        let pending: Vec<(EntryId, Option<CallId>)> = state.pending_approvals.iter().map(|(k, v)| (*k, v.clone())).collect();
        for (req, call) in &pending {
            if m.asked.contains(req) {
                continue;
            }
            if let Some(c) = call
                && tool_of(c) == Some(tool)
            {
                m.asked.insert(*req);
                return (c.0.clone(), false);
            }
        }
        if let Some((req, _)) = pending.iter().find(|(r, c)| c.is_none() && !m.asked.contains(r)) {
            m.asked.insert(*req);
        }
        match state.open_calls.iter().rev().find(|c| tool_of(c) == Some(tool)) {
            Some(c) => (c.0.clone(), false),
            None => self.synthetic(m),
        }
    }

    fn synthetic(&self, m: &mut Mapper) -> (String, bool) {
        m.synthetic += 1;
        (format!("approval-{}", m.synthetic), true)
    }

    /// Puts one approval card to the client. Operator authority is the ACP client's (process
    /// ownership); the agent never answers its own card. A card raised after the client cancelled
    /// the turn is answered `cancelled` without asking.
    fn ask(self: &Arc<Self>, m: &mut Mapper, card: aho_policy::ApprovalCard, reply: oneshot::Sender<Decided>) {
        let (call, synthetic) = self.card_call(m, &card.tool);
        if lock(&self.turn).as_ref().is_some_and(|t| t.cancelled) {
            if reply.send(Decided::cancelled()).is_err() {
                (self.diag)(&format!("session {}: a call stopped waiting for its approval (turn over)", self.id));
            }
            return;
        }
        let kind = map::kind_of(card.class, &card.tool);
        let title = map::title_of(&card.tool, &card.args);
        if synthetic {
            let mut tc = m.tool_call(&CallId(call.clone()), &card.tool, &card.args, card.class);
            tc.title = title.clone();
            self.send(SessionUpdate::ToolCall(tc));
        }
        let req = RequestPermissionRequest {
            session_id: self.id.clone(),
            tool_call: ToolCallUpdate {
                tool_call_id: call.clone(),
                kind: Some(kind),
                status: Some(ToolCallStatus::Pending),
                title: Some(title),
                name: Some(card.tool.clone()),
                content: Some(vec![ToolCallContent::text(card.rendered.clone())]),
                raw_input: Some(card.args.clone()),
                ..ToolCallUpdate::default()
            },
            options: permission_options(),
            meta: None,
        };
        // Enqueued now, behind every update already sent for this call.
        let answer = self.peer.call::<_, RequestPermissionResponse>(methods::SESSION_REQUEST_PERMISSION, &req);
        let me = self.clone();
        tokio::spawn(async move {
            let decided = match answer.await {
                Ok(r) => decided(&r.outcome).unwrap_or_else(|| {
                    (me.diag)(&format!("session {}: the client picked an option aho did not offer; the call is refused", me.id));
                    Decided::UNAVAILABLE
                }),
                Err(e) => {
                    (me.diag)(&format!("session {}: the permission request failed ({e}); the call is refused", me.id));
                    Decided::UNAVAILABLE
                }
            };
            if synthetic {
                let status = if decided.outcome == aho_log::ApprovalOutcome::AllowedOnce {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Failed
                };
                me.send(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                    tool_call_id: call,
                    status: Some(status),
                    ..ToolCallUpdate::default()
                }));
            }
            if reply.send(decided).is_err() {
                (me.diag)(&format!("session {}: the call stopped waiting before the answer arrived (turn over)", me.id));
            }
        });
    }

    /// `session/cancel`: aborts the turn. The Session stops its running calls and ends the turn at
    /// the next boundary (between steps, mid model request, or while waiting) without calling
    /// the model again; the prompt then answers `cancelled`.
    pub(crate) fn cancel(&self) {
        let mut t = lock(&self.turn);
        let Some(turn) = t.as_mut() else { return };
        if !turn.cancelled {
            turn.cancelled = true;
            self.session.abort();
        }
    }

    /// `session/prompt`: one turn, answered after its updates.
    pub(crate) async fn prompt(self: Arc<Self>, text: String) -> Result<PromptResponse, RpcError> {
        {
            let mut t = lock(&self.turn);
            if t.is_some() {
                return Err(RpcError::invalid_request(format!(
                    "a prompt is already running in session {}; wait for its stopReason or send session/cancel first",
                    self.id
                )));
            }
            *t = Some(Turn::default());
        }
        let guard = TurnGuard { live: self.clone(), done: false };
        let answer = self.clone().run_turn(text).await;
        drop(guard.finish());
        answer
    }

    async fn run_turn(self: Arc<Self>, text: String) -> Result<PromptResponse, RpcError> {
        let expected = self.session.fold().map_err(|e| RpcError::internal(e.to_string()))?.turns + 1;
        let s = self.session.clone();
        let outcome = tokio::spawn(async move { s.prompt(text, None).await })
            .await
            .map_err(|e| RpcError::internal(format!("the turn task failed inside aho: {e}")))?;
        let transition = match outcome {
            Ok(t) => t,
            Err(SessionError::Busy) => {
                return Err(RpcError::invalid_request(format!(
                    "session {} is already running a turn started elsewhere; wait for it to finish",
                    self.id
                )));
            }
            Err(e) => return Err(RpcError::internal(e.to_string())),
        };
        let mut ended = self.turn_end.subscribe();
        if ended.wait_for(|t| *t >= expected).await.is_err() {
            return Err(RpcError::internal("the session's update stream stopped before the turn's updates were sent"));
        }
        let cancelled = lock(&self.turn).as_ref().is_some_and(|t| t.cancelled);
        if cancelled {
            self.fail_open_calls(expected);
        }
        let stop_reason = map::stop_reason(&transition, cancelled, self.last_stop(expected)).map_err(RpcError::internal)?;
        Ok(PromptResponse { stop_reason, meta: None })
    }

    /// A cancelled turn can end with its own calls still open (their results land later):
    /// tell the client they are over before the `cancelled` answer (prompt-turn §Cancellation).
    /// Calls earlier turns left running are not this cancel's to report.
    fn fail_open_calls(&self, turn: u64) {
        let Ok(state) = self.session.fold() else { return };
        let Some(head) = self.session.log().head() else { return };
        let Ok(entries) = self.session.log().read_branch(head) else { return };
        let mut mine = Vec::new();
        for e in entries.iter().rev() {
            match &e.kind {
                EntryKind::ToolCall { call, .. } if state.open_calls.contains(call) => mine.push(call.clone()),
                EntryKind::TurnStart { turn: t } if t.0 == turn => break,
                _ => {}
            }
        }
        for call in mine.into_iter().rev() {
            self.send(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                tool_call_id: call.0,
                status: Some(ToolCallStatus::Failed),
                content: Some(vec![ToolCallContent::text("Cancelled by the client before it finished.")]),
                ..ToolCallUpdate::default()
            }));
        }
    }

    /// The last `Assistant/Message` stop inside turn `turn`.
    fn last_stop(&self, turn: u64) -> Option<AhoStop> {
        let head = self.session.log().head()?;
        let entries = self.session.log().read_branch(head).ok()?;
        for e in entries.iter().rev() {
            match &e.kind {
                EntryKind::AssistantMessage { stop, .. } => return Some(*stop),
                EntryKind::TurnStart { turn: t } if t.0 == turn => return None,
                _ => {}
            }
        }
        None
    }
}

/// Clears the turn slot when the prompt call ends; a call dropped mid-turn (`$/cancel_request`,
/// connection gone) cancels the turn it started.
struct TurnGuard {
    live: Arc<Live>,
    done: bool,
}

impl TurnGuard {
    fn finish(mut self) -> Self {
        self.done = true;
        self
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if !self.done {
            self.live.cancel();
        }
        *lock(&self.live.turn) = None;
    }
}

async fn next_card(rx: &mut Option<mpsc::Receiver<ApprovalRequest>>) -> Option<ApprovalRequest> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

async fn forward(
    live: Arc<Live>,
    mut events: broadcast::Receiver<SessionEvent>,
    mut cards: Option<mpsc::Receiver<ApprovalRequest>>,
    mut m: Mapper,
    mut last: Option<EntryId>,
) {
    let mut stop = live.stop.subscribe();
    loop {
        tokio::select! {
            biased;
            _ = stop.changed() => break,
            ev = events.recv() => match ev {
                Ok(ev) => live.on_event(&mut m, ev, &mut last),
                Err(RecvError::Lagged(_)) => live.recover(&mut m, &mut last),
                Err(RecvError::Closed) => break,
            },
            card = next_card(&mut cards) => match card {
                Some((card, reply)) => {
                    // The call's `Tool/Call` and `Approval/Requested` were broadcast before the
                    // card was sent: flush them so the client knows the call it is asked about.
                    loop {
                        match events.try_recv() {
                            Ok(ev) => live.on_event(&mut m, ev, &mut last),
                            Err(TryRecvError::Lagged(_)) => live.recover(&mut m, &mut last),
                            Err(_) => break,
                        }
                    }
                    live.ask(&mut m, card, reply);
                }
                None => cards = None,
            },
        }
    }
    // Nothing more will be forwarded: release every prompt waiting for its turn's updates.
    live.turn_end.send_replace(u64::MAX);
}

fn permission_options() -> Vec<PermissionOption> {
    let o = |id: &str, name: &str, kind| PermissionOption { option_id: id.into(), name: name.into(), kind, meta: None };
    vec![
        o(ALLOW_ONCE, "Allow once", PermissionOptionKind::AllowOnce),
        o(ALLOW_SESSION, "Allow for this session", PermissionOptionKind::AllowAlways),
        o(REJECT_ONCE, "Reject", PermissionOptionKind::RejectOnce),
    ]
}

/// The client's choice as an aho decision; `None` for an option aho did not offer.
fn decided(o: &RequestPermissionOutcome) -> Option<Decided> {
    match o {
        RequestPermissionOutcome::Cancelled => Some(Decided::cancelled()),
        RequestPermissionOutcome::Selected { option_id } => match option_id.as_str() {
            ALLOW_ONCE => Some(Decided::allowed(GrantScope::Once)),
            ALLOW_SESSION => Some(Decided::allowed(GrantScope::Session)),
            REJECT_ONCE => Some(Decided::rejected()),
            _ => None,
        },
    }
}

/// Live sessions of one connection, by ACP session id.
#[derive(Default)]
pub(crate) struct Lives {
    map: Mutex<BTreeMap<String, Arc<Live>>>,
}

impl Lives {
    pub(crate) fn get(&self, id: &str) -> Option<Arc<Live>> {
        lock(&self.map).get(id).cloned()
    }
    pub(crate) fn insert(&self, live: Arc<Live>) {
        lock(&self.map).insert(live.id.clone(), live);
    }
    pub(crate) fn contains(&self, id: &str) -> bool {
        lock(&self.map).contains_key(id)
    }
    pub(crate) fn stop_all(&self) {
        for l in lock(&self.map).values() {
            l.stop();
        }
    }
}
