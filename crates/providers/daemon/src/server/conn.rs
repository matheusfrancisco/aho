//! Daemon connection I/O: socket binding, frame helpers, request params, event and approval forwarding
//! (split out of server.rs for the file-size ratchet, doc 12).

use super::*;

#[allow(clippy::disallowed_methods, reason = "authority: daemon-socket-bind")]
pub(super) async fn bind(path: &Path) -> Result<UnixListener, String> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            return Err(format!(
                "{} exists and is not a socket, so we did not touch it or start. Remove it, or pass another --socket",
                path.display()
            ));
        }
        if UnixStream::connect(path).await.is_ok() {
            return Err(format!(
                "another daemon already serves {}, so we did not start a second one. Stop it with `daemon.shutdown`, or pass another --socket",
                path.display()
            ));
        }
        std::fs::remove_file(path).map_err(|e| {
            format!(
                "{} is a stale socket (nothing answers on it) and could not be removed ({e}). Remove it by hand and start again",
                path.display()
            )
        })?;
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {} for the socket ({e}), so the daemon did not start", parent.display()))?;
    }
    let listener = UnixListener::bind(path).map_err(|e| {
        format!("could not listen on {} ({e}), so the daemon did not start. Check the directory permissions and keep the path under about 100 bytes", path.display())
    })?;
    // Attaching carries operator authority: only this user may connect. Fail closed if we cannot say so.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
        let _ = std::fs::remove_file(path);
        format!("could not restrict {} to this user ({e}), so the daemon did not start: anyone able to reach the socket would hold operator authority", path.display())
    })?;
    Ok(listener)
}

#[allow(clippy::disallowed_methods, reason = "authority: daemon-list-logs")]
pub(super) fn list_logs(dir: &Path) -> Vec<(SessionId, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(SessionId, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension()? != "jsonl" {
                return None;
            }
            let id = u128::from_str_radix(p.file_stem()?.to_str()?, 16).ok()?;
            Some((SessionId(id), p))
        })
        .collect();
    out.sort();
    out
}

/// The turn open at the head of `log`; `Err` says why the log cannot be read ("could not be
/// read …" / "does not fold …").
pub(super) fn open_turn(log: &dyn LogStore) -> Result<Option<TurnId>, String> {
    let entries = match log.head() {
        Some(h) => log.read_branch(h).map_err(|e| format!("could not be read ({e})"))?,
        None => return Ok(None),
    };
    aho_log::fold(&entries, &Default::default()).map(|s| s.turn).map_err(|e| format!("does not fold ({e})"))
}

#[allow(clippy::disallowed_methods, reason = "authority: daemon-reload-binary")]
pub(super) fn executable(path: &Path) -> Result<(), String> {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() && m.permissions().mode() & 0o111 != 0 => Ok(()),
        Ok(_) => Err(format!(
            "{} is not an executable file, so we did not reload and nothing was interrupted. Point `binary` at a built aho binary",
            path.display()
        )),
        Err(e) => Err(format!(
            "{} cannot be used ({e}), so we did not reload and nothing was interrupted. Point `binary` at a built aho binary",
            path.display()
        )),
    }
}

pub(super) fn parse<T: DeserializeOwned>(method: &str, params: Value) -> Result<T, String> {
    let params = if params.is_null() { Value::Object(Default::default()) } else { params };
    serde_json::from_value(params)
        .map_err(|e| format!("`{method}` params are not valid ({e}), so we did nothing. Fix them and send it again"))
}

pub(super) fn send(tx: &Tx, frame: ServerFrame) {
    let _ = tx.send(Out::Frame(frame));
}

pub(super) async fn flush(tx: &Tx) {
    let (ack, done) = oneshot::channel();
    if tx.send(Out::Flushed(ack)).is_ok() {
        let _ = done.await;
    }
}

pub(super) fn not_open(sid: SessionId) -> String {
    format!("session {} is not open in this daemon. Open it with `session.open` first", sid.0)
}

pub(super) const CLOSING: &str = "the daemon is reloading or shutting down, so we did not do that. Reconnect in a moment and try again";

#[derive(Deserialize)]
pub(super) struct OpenParams {
    #[serde(default)]
    pub(super) id: Option<SessionId>,
}

#[derive(Deserialize)]
pub(super) struct SessionParams {
    pub(super) session: SessionId,
}

#[derive(Deserialize)]
pub(super) struct AttachParams {
    pub(super) session: SessionId,
    #[serde(default)]
    pub(super) client_instance: Option<String>,
    #[serde(default)]
    pub(super) has_local_history: bool,
    #[serde(default)]
    pub(super) from: Option<EntryId>,
    /// `false`: decide ownership only; stream nothing until `session.subscribe`.
    #[serde(default = "yes")]
    pub(super) watch: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
pub(super) struct SubscribeParams {
    pub(super) session: SessionId,
    #[serde(default)]
    pub(super) from: Option<EntryId>,
}

#[derive(Deserialize)]
pub(super) struct PromptParams {
    pub(super) session: SessionId,
    pub(super) text: String,
    #[serde(default)]
    pub(super) client_key: Option<String>,
    /// Reply when the turn ends instead of when it starts.
    #[serde(default)]
    pub(super) wait: bool,
}

#[derive(Deserialize)]
pub(super) struct SteerParams {
    pub(super) session: SessionId,
    pub(super) text: String,
    #[serde(default)]
    pub(super) urgent: bool,
}

#[derive(Deserialize)]
pub(super) struct CommandParams {
    pub(super) session: SessionId,
    pub(super) kind: EntryKind,
}

#[derive(Deserialize)]
pub(super) struct CallParams {
    pub(super) session: SessionId,
    pub(super) method: String,
    #[serde(default)]
    pub(super) params: Value,
}

#[derive(Deserialize)]
pub(super) struct AnswerParams {
    pub(super) id: u64,
    #[serde(default)]
    pub(super) allow: Option<bool>,
    #[serde(default)]
    pub(super) outcome: Option<String>,
    #[serde(default)]
    pub(super) scope: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ReloadParams {
    #[serde(default)]
    pub(super) binary: Option<PathBuf>,
    #[serde(default)]
    pub(super) session: Option<SessionId>,
}

/// Cleans up after a connection however its task ends (end of file, kick, abort).
pub(super) struct ConnGuard {
    pub(super) d: Arc<Daemon>,
    pub(super) id: ConnId,
    pub(super) writer: JoinHandle<()>,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.writer.abort();
        let streams = {
            let mut st = self.d.st();
            let streams: Vec<JoinHandle<()>> =
                st.conns.remove(&self.id).map(|c| c.attachments.into_values().map(|s| s.task).collect()).unwrap_or_default();
            for l in st.sessions.values_mut() {
                if let Some(o) = l.owner.as_mut().filter(|o| o.conn == self.id) {
                    o.live = false;
                }
            }
            st.approvals.retain(|_, a| a.conn != self.id);
            streams
        };
        for s in streams {
            s.abort();
        }
        self.d.abandon_mutations(self.id);
    }
}

pub(super) async fn write_loop(mut wr: OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<Out>) {
    while let Some(out) = rx.recv().await {
        match out {
            Out::Frame(f) => {
                if wire::write_frame(&mut wr, &f).await.is_err() {
                    return;
                }
            }
            Out::Flushed(ack) => {
                let _ = ack.send(());
            }
        }
    }
}

pub(super) fn stream_payload(s: &StreamEvent) -> Value {
    match s {
        StreamEvent::Text(t) => json!({ "kind": "text", "text": t }),
        StreamEvent::Thinking(t) => json!({ "kind": "thinking", "text": t }),
        StreamEvent::ToolUseStart { id, name } => json!({ "kind": "tool_use_start", "id": id, "name": name }),
        StreamEvent::ToolUseEnd { id, name, args } => json!({ "kind": "tool_use_end", "id": id, "name": name, "args": args }),
        StreamEvent::Usage(u) => json!({ "kind": "usage", "usage": u }),
    }
}

pub(super) fn entry_frame(sid: SessionId, e: &Entry) -> Out {
    let payload = match serde_json::to_value(e) {
        Ok(entry) => json!({ "session": sid, "entry": entry }),
        Err(err) => {
            json!({ "session": sid, "entry_id": e.id, "message": format!("entry {} could not be encoded ({err}); read it from the log", e.id.0) })
        }
    };
    Out::Frame(ServerFrame::event("entry", payload))
}

/// Streams one session to one connection: entries after `from` first, then live events. Entries
/// appended without a broadcast (by the daemon, by the approval wrapper) are filled in from the
/// log when the next broadcast entry arrives, so the stream has no gaps. `seen` holds the newest
/// entry sent, for moving the stream onto a rebooted session.
pub(super) async fn forward(
    tx: Tx,
    session: Arc<Session>,
    mut rx: broadcast::Receiver<SessionEvent>,
    from: Option<EntryId>,
    replay: Vec<Entry>,
    seen: Arc<Mutex<Option<EntryId>>>,
) {
    let sid = session.id;
    let mut last = from;
    let sent = |last: &mut Option<EntryId>, e: &Entry| -> bool {
        *last = Some(e.id);
        let ok = tx.send(entry_frame(sid, e)).is_ok();
        *lock(&seen) = *last;
        ok
    };
    for e in &replay {
        if !sent(&mut last, e) {
            return;
        }
    }
    loop {
        let out = match rx.recv().await {
            Ok(SessionEvent::Entry(e)) => {
                if last.is_some_and(|l| e.id <= l) {
                    continue;
                }
                for g in session.subscribe_from(last).iter().filter(|g| g.id <= e.id) {
                    if !sent(&mut last, g) {
                        return;
                    }
                }
                continue;
            }
            Ok(SessionEvent::Stream(s)) => ServerFrame::event("stream", json!({ "session": sid, "event": stream_payload(&s) })),
            Ok(SessionEvent::Idle { turn, transition }) => {
                ServerFrame::event("idle", json!({ "session": sid, "turn": turn.0, "transition": transition }))
            }
            Ok(SessionEvent::Error(message)) => ServerFrame::event("error", json!({ "session": sid, "message": message })),
            Err(broadcast::error::RecvError::Lagged(_)) => {
                for g in session.subscribe_from(last) {
                    if !sent(&mut last, &g) {
                        return;
                    }
                }
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if tx.send(Out::Frame(out)).is_err() {
            return;
        }
    }
}

/// Cards go to the session's live owner as `approval.request` with the fields every surface
/// renders (05); with no owner the reply slot is dropped, which the channel answers as
/// `Unavailable` (fail closed).
pub(super) async fn forward_approvals(d: Arc<Daemon>, sid: SessionId, mut requests: mpsc::Receiver<ApprovalRequest>) {
    while let Some((card, reply)) = requests.recv().await {
        let (id, tx, cap, epoch_next) = {
            let mut st = d.st();
            if st.closing {
                continue;
            }
            let Some(live) = st.sessions.get(&sid) else { continue };
            let reg = live.session.registry();
            let cap = reg
                .tools
                .iter()
                .find(|(_, t, _)| t.name == card.tool)
                .map(|(c, _, _)| c.name.to_string())
                .or_else(|| reg.dynamic_tools.iter().find(|(_, s, _)| s.name == card.tool).map(|(c, _, _)| c.name.to_string()));
            let epoch_next = live.session.policy_view().epoch.0 + 1;
            let owner = live.owner.as_ref().filter(|o| o.live).map(|o| o.conn);
            let Some((conn, tx)) = owner.and_then(|c| st.conns.get(&c).map(|x| (c, x.tx.clone()))) else {
                continue;
            };
            st.approvals.retain(|_, a| !a.reply.is_closed());
            st.next_approval += 1;
            let id = st.next_approval;
            st.approvals.insert(id, PendingApproval { session: sid, conn, preset: surface_print::preset_cut(&card), reply });
            (id, tx, cap, epoch_next)
        };
        send(
            &tx,
            ServerFrame::event(
                "approval.request",
                json!({
                    "id": id,
                    "session": sid,
                    "tool": card.tool,
                    "cap": cap,
                    "class": card.class,
                    "args": card.args,
                    "rendered": card.rendered,
                    "why": card.rendered,
                    "reason": surface_print::reason_label(&card.reason),
                    "world": card.world.name,
                    "enforced_by": card.world.enforced_by,
                    "suggested_cut": card.suggested_cut,
                    "preset_cut": surface_print::preset_cut(&card),
                    "epoch_next": epoch_next,
                }),
            ),
        );
    }
}

pub(super) fn running_jobs(s: &Session) -> Vec<String> {
    let calls = s.fold().map(|st| st.calls).unwrap_or_default();
    s.ops()
        .running()
        .into_iter()
        .map(|(op, call, _)| {
            let tool = calls.get(&call).map(|c| c.0.as_str()).unwrap_or("tool");
            format!("job {}: {tool} (call {})", op.0, call.0)
        })
        .collect()
}
