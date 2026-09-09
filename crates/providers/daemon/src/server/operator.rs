//! What a driving client does to a live session beyond prompting (12, 05, 03): stream it from an
//! entry (`session.subscribe`), append an operator entry (`session.command`), call a method the
//! host serves over it (`session.call`), boot it again so it runs under what the operator just
//! recorded (`session.reopen`), and answer its approval cards. Everything that changes a session
//! needs the connection to drive it ([`Daemon::owner_check`]); watching does not.

use super::*;

/// Kinds an operator command may append: configuration and plan records (03), operator commands
/// and notes (05), invariant records (13), and capability-owned records (`Custom`, e.g.
/// `schedule/Cancelled`). The turn machinery — turns, steps, messages, calls, results,
/// operations, approvals, attachments, recovery, compaction — is written by the session only.
fn operator_kind(k: &EntryKind) -> bool {
    matches!(
        k,
        EntryKind::ConfigResolved { .. }
            | EntryKind::CapDelta { .. }
            | EntryKind::PolicyEpoch { .. }
            | EntryKind::PlanSealed { .. }
            | EntryKind::LoadoutDiagnostic { .. }
            | EntryKind::CommandRun { .. }
            | EntryKind::CommandDone { .. }
            | EntryKind::Note { .. }
            | EntryKind::InvariantRegistered { .. }
            | EntryKind::InvariantChecked { .. }
            | EntryKind::InvariantMutated { .. }
            | EntryKind::Custom { .. }
    )
}

/// Kinds that change what the session booted with: they wait until no turn runs, and
/// `session.reopen` boots the session under them.
fn plan_kind(k: &EntryKind) -> bool {
    matches!(
        k,
        EntryKind::ConfigResolved { .. }
            | EntryKind::CapDelta { .. }
            | EntryKind::PolicyEpoch { .. }
            | EntryKind::PlanSealed { .. }
            | EntryKind::LoadoutDiagnostic { .. }
    )
}

/// Stops a stream task and waits until it has stopped, so the entry it sent last is final.
async fn stopped(s: Stream) -> Option<EntryId> {
    s.task.abort();
    if let Err(e) = s.task.await
        && e.is_panic()
    {
        eprintln!("aho serve: a session stream failed ({e}); the client resubscribes from its last entry");
    }
    *lock(&s.last)
}

impl Daemon {
    /// Starts an operator transaction before any config write or audit record. Admission uses
    /// the session lock, not the daemon's foreground-task bookkeeping: drivers bypass that.
    pub(super) fn begin_mutation(&self, conn: ConnId, sid: SessionId) -> Result<Value, String> {
        let mut st = self.st();
        Daemon::owner_check(&st, conn, sid)?;
        let live = st.sessions.get_mut(&sid).ok_or_else(|| not_open(sid))?;
        if let Some(m) = &live.mutation {
            return if m.conn == conn {
                Ok(json!({ "session": sid, "head": live.session.log().head() }))
            } else {
                Err("another client's mutation is being closed; open the session again once it closes".into())
            };
        }
        if live.turn.is_some() {
            return Err("a turn is running, so the mutation did not start; try again when idle".into());
        }
        let guard = live.session.try_mutation().map_err(|e| format!("the mutation did not start: {e}"))?;
        let jobs = running_jobs(&live.session);
        if !jobs.is_empty() {
            return Err(format!("background work is running ({}), so the mutation did not start", jobs.join("; ")));
        }
        live.mutation = Some(Mutation { conn, _guard: guard, head: live.session.log().head() });
        Ok(json!({ "session": sid, "head": live.session.log().head() }))
    }

    fn release_unchanged(&self, conn: ConnId, sid: SessionId) {
        let mut st = self.st();
        if let Some(live) = st.sessions.get_mut(&sid)
            && live.mutation.as_ref().is_some_and(|m| m.conn == conn && m.head == live.session.log().head())
        {
            live.mutation = None;
        }
    }

    /// An interrupted transaction never restarts drivers against a partially changed log.
    /// Close dirty sessions under the file lock; the next open boots the recorded plan.
    pub(super) fn abandon_mutations(self: &Arc<Self>, conn: ConnId) {
        let d = self.clone();
        tokio::spawn(async move {
            let _files = d.files.lock().await;
            let removed = {
                let mut st = d.st();
                let ids: Vec<_> = st
                    .sessions
                    .iter()
                    .filter_map(|(sid, live)| {
                        (live.mutation.as_ref().is_some_and(|m| m.conn == conn)
                            && !live.owner.as_ref().is_some_and(|o| o.conn == conn && o.live))
                        .then_some(*sid)
                    })
                    .collect();
                let mut removed = Vec::new();
                for sid in ids {
                    let unchanged =
                        st.sessions.get(&sid).is_some_and(|l| l.mutation.as_ref().is_some_and(|m| m.head == l.session.log().head()));
                    if unchanged {
                        if let Some(live) = st.sessions.get_mut(&sid) {
                            live.mutation = None;
                        }
                        continue;
                    }
                    if let Some(live) = st.sessions.remove(&sid) {
                        live.approvals.abort();
                        removed.push((sid, live));
                    }
                    for c in st.conns.values_mut() {
                        c.attached.remove(&sid);
                        if let Some(stream) = c.attachments.remove(&sid) {
                            stream.task.abort();
                        }
                    }
                    st.problems
                        .insert(sid, "an operator disconnected during a mutation; open the session again to boot its recorded plan".into());
                }
                removed
            };
            for (sid, live) in removed {
                for problem in d.host.closing(sid).await {
                    eprintln!("aho serve: session {}: {problem}", sid.0);
                }
                // The old session's admission guard outlives all its drivers.
                drop(live);
            }
        });
    }

    /// Streams `session` to `conn` (entries after `from`, `replay` first) in place of any stream
    /// it had for that session.
    pub(super) fn start_stream(
        &self,
        conn: ConnId,
        tx: &Tx,
        session: Arc<Session>,
        rx: broadcast::Receiver<SessionEvent>,
        from: Option<EntryId>,
        replay: Vec<Entry>,
    ) {
        let sid = session.id;
        let last = Arc::new(Mutex::new(from));
        let task = tokio::spawn(forward(tx.clone(), session, rx, from, replay, last.clone()));
        let previous = self.st().conns.get_mut(&conn).and_then(|c| c.attachments.insert(sid, Stream { task, last }));
        if let Some(p) = previous {
            p.task.abort();
        }
    }

    /// `session.prompt`: starts a turn. With `wait` the reply is the turn's end
    /// (`result{session, transition}` or an `error`), as the rpc surface answers; without it the
    /// reply is `accepted` and the end arrives as the stream's `idle`.
    pub(super) fn prompt(self: &Arc<Self>, conn: ConnId, id: u64, tx: &Tx, p: PromptParams) -> Result<(), String> {
        let sid = p.session;
        let mut st = self.st();
        Daemon::owner_check(&st, conn, sid)?;
        let live = st.sessions.get(&sid).ok_or_else(|| not_open(sid))?;
        if live.mutation.is_some() {
            return Err("an operator mutation is pending; reopen the session before starting another turn".into());
        }
        if live.turn.is_some() {
            return Err(format!(
                "a turn is already running in session {}, so we did not start another. Steer it with `session.steer`, or wait for its `idle` event",
                sid.0
            ));
        }
        let session = live.session.clone();
        let (out, wait) = (tx.clone(), p.wait);
        self.spawn_turn(&mut st, sid, async move {
            match session.prompt(p.text, p.client_key).await {
                Ok(t) if wait => send(&out, ServerFrame::reply(id, "result", json!({ "session": sid, "transition": t }))),
                Ok(_) => {}
                Err(e) if wait => send(&out, ServerFrame::error(Some(id), e.to_string())),
                Err(e) => send(&out, ServerFrame::event("error", json!({ "session": sid, "message": e.to_string() }))),
            }
        });
        if !wait {
            send(tx, ServerFrame::reply(id, "accepted", json!({ "session": sid })));
        }
        Ok(())
    }

    /// `session.subscribe{session, from?}`: (re)starts this connection's stream of a session it
    /// attached to: the entries after `from` (all when absent), then the reply
    /// `{session, replayed, head}`, then live events.
    pub(super) async fn subscribe(self: &Arc<Self>, conn: ConnId, id: u64, tx: &Tx, p: SubscribeParams) -> Result<(), String> {
        let sid = p.session;
        let (session, previous) = {
            let mut st = self.st();
            if st.closing {
                return Err(CLOSING.to_string());
            }
            let session = st.sessions.get(&sid).map(|l| l.session.clone()).ok_or_else(|| not_open(sid))?;
            let c = st.conns.get_mut(&conn).filter(|c| c.attached.contains(&sid)).ok_or_else(|| {
                format!("this client has not attached to session {}, so there is nothing to stream. Attach first (`session.attach`)", sid.0)
            })?;
            (session, c.attachments.remove(&sid))
        };
        // The previous stream stops first, so nothing it sends lands after the replay.
        if let Some(s) = previous {
            stopped(s).await;
        }
        let rx = session.subscribe();
        let replay = session.subscribe_from(p.from);
        for e in &replay {
            if tx.send(entry_frame(sid, e)).is_err() {
                return Ok(());
            }
        }
        let last = replay.last().map(|e| e.id).or(p.from);
        send(tx, ServerFrame::reply(id, "result", json!({ "session": sid, "replayed": replay.len(), "head": session.log().head() })));
        self.start_stream(conn, tx, session, rx, last, Vec::new());
        Ok(())
    }

    /// `session.command{session, kind}`: appends one operator entry (by `Operator`) to a session
    /// this connection drives. Refused: kinds only the session writes, plan kinds while a turn
    /// runs, and an entry the session's fold would reject (checked before anything is written).
    pub(super) fn command(&self, conn: ConnId, p: CommandParams) -> Result<Value, String> {
        let sid = p.session;
        let tag = p.kind.tag();
        if !operator_kind(&p.kind) {
            return Err(format!(
                "`{tag}` entries are written by the session itself, never by an operator command, so nothing was appended. Operator commands append configuration, plan, command, note, invariant and capability (`custom`) records"
            ));
        }
        let changing = plan_kind(&p.kind);
        if changing {
            self.begin_mutation(conn, sid)?;
        }
        let result = self.append_command(conn, p);
        if changing && result.is_err() {
            self.release_unchanged(conn, sid);
        }
        result
    }

    fn append_command(&self, conn: ConnId, p: CommandParams) -> Result<Value, String> {
        let sid = p.session;
        let tag = p.kind.tag();
        let session = {
            let st = self.st();
            Daemon::owner_check(&st, conn, sid)?;
            let live = st.sessions.get(&sid).ok_or_else(|| not_open(sid))?;
            if plan_kind(&p.kind) && live.turn.is_some() {
                return Err(format!(
                    "a turn is running in session {}, so the `{tag}` entry was not appended: a plan changes between turns. Send it again once the session is idle",
                    sid.0
                ));
            }
            live.session.clone()
        };
        let state =
            session.fold().map_err(|e| format!("session {} does not fold ({e}), so nothing was appended. Inspect its log", sid.0))?;
        let probe = Entry {
            id: EntryId(state.head.map_or(0, |h| h.0 + 1)),
            parent: state.head,
            prev_hash: Hash::of(b""),
            hash: Hash::of(b""),
            at: Timestamp::now(),
            by: Principal::Operator,
            kind: p.kind.clone(),
        };
        aho_log::step(state, &probe, &session.registry().entry_kinds).map_err(|e| {
            format!(
                "the session log would not fold with this `{tag}` entry ({e}), so nothing was appended. Fix the command and send it again"
            )
        })?;
        let e = session
            .append_entry(Principal::Operator, p.kind)
            .map_err(|e| format!("could not append to the log of session {} ({e}); nothing was recorded", sid.0))?;
        Ok(json!({ "session": sid, "entry": e.id }))
    }

    /// `session.call{session, method, params}`: the host's method over a session this
    /// connection drives.
    pub(super) async fn host_call(self: &Arc<Self>, conn: ConnId, p: CallParams) -> Result<Value, String> {
        let changing = self.host.mutates(&p.method, &p.params);
        if changing {
            self.begin_mutation(conn, p.session)?;
        }
        let session = self.owned(conn, p.session)?;
        let result = self.host.call(&session, &p.method, p.params).await;
        if !changing {
            return result;
        }
        if result.is_err() {
            self.release_unchanged(conn, p.session);
        }
        let pending = self.st().sessions.get(&p.session).is_some_and(|l| l.mutation.is_some());
        if pending {
            let reopened = self.reopen(conn, p.session).await;
            match (result, reopened) {
                (Ok(value), Ok(_)) => Ok(value),
                (Err(error), Ok(_)) => Err(error),
                (Ok(_), Err(error)) => Err(format!("the mutation was recorded, but reopening failed: {error}")),
                (Err(error), Err(reopen)) => Err(format!("{error}; reopening also failed: {reopen}")),
            }
        } else {
            result
        }
    }

    /// `session.reopen{session}`: the host closes the session and boots it again over the same
    /// log, so it runs under what was recorded since it booted (an operator's plan change). The
    /// owner stays; every stream moves to the new session from the entry it sent last. Refused
    /// while a turn or a background job runs. A boot that fails leaves the session closed here.
    pub(super) async fn reopen(self: &Arc<Self>, conn: ConnId, sid: SessionId) -> Result<Value, String> {
        let _files = self.files.lock().await;
        self.begin_mutation(conn, sid)?;
        let old = {
            let st = self.st();
            Daemon::owner_check(&st, conn, sid)?;
            let live = st.sessions.get(&sid).ok_or_else(|| not_open(sid))?;
            if live.turn.is_some() {
                return Err(format!("a turn is running in session {}, so it was not reopened. Reopen it once the session is idle", sid.0));
            }
            let jobs = running_jobs(&live.session);
            if !jobs.is_empty() {
                return Err(format!(
                    "session {} has background work running ({}), so it was not reopened: a new boot would lose track of it. Reopen it once they finish",
                    sid.0,
                    jobs.join("; ")
                ));
            }
            live.session.clone()
        };
        let problems = self.host.closing(sid).await;
        let (approval, requests) = ChannelApproval::new(16);
        let session = match self.host.boot(sid, old.log().clone(), Arc::new(approval)).await {
            Ok(s) => s,
            Err(e) => {
                let (streams, forwarder) = {
                    let mut st = self.st();
                    let forwarder = st.sessions.remove(&sid).map(|l| l.approvals);
                    st.problems.insert(sid, format!("the session could not boot again after an operator command ({e})"));
                    let mut streams = Vec::new();
                    for c in st.conns.values_mut() {
                        c.attached.remove(&sid);
                        streams.extend(c.attachments.remove(&sid));
                    }
                    (streams, forwarder)
                };
                for s in streams {
                    s.task.abort();
                }
                if let Some(f) = forwarder {
                    f.abort();
                }
                return Err(format!(
                    "session {} could not boot again ({e}), so it is closed in this daemon; its log keeps everything. Fix that, then open it with `session.open`",
                    sid.0
                ));
            }
        };
        let approvals = tokio::spawn(forward_approvals(self.clone(), sid, requests));
        let moved = {
            let mut st = self.st();
            if let Some(live) = st.sessions.get_mut(&sid) {
                live.session = session.clone();
                live.mutation = None;
                std::mem::replace(&mut live.approvals, approvals).abort();
            }
            let mut moved = Vec::new();
            for (id, c) in st.conns.iter_mut() {
                if let Some(s) = c.attachments.remove(&sid) {
                    moved.push((*id, c.tx.clone(), s));
                }
            }
            moved
        };
        for (id, tx, s) in moved {
            let from = stopped(s).await;
            let rx = session.subscribe();
            let replay = session.subscribe_from(from);
            self.start_stream(id, &tx, session.clone(), rx, from, replay);
        }
        Ok(json!({ "session": sid, "head": session.log().head(), "problems": problems }))
    }

    /// `approval.answer{id, allow | outcome, scope?}`: `outcome` is `allow`, `reject` or `cancel`
    /// (a cancel reads to the model unlike a rejection, 04); `allow: bool` is the short form.
    pub(super) async fn answer(&self, conn: ConnId, p: AnswerParams) -> Result<Value, String> {
        if p.outcome.as_deref() == Some("preset") {
            return self.answer_preset(conn, p.id).await;
        }
        let scope = match p.scope.as_deref() {
            None | Some("once") => GrantScope::Once,
            Some("session") => GrantScope::Session,
            Some("until_revoked") => GrantScope::UntilRevoked,
            Some(other) => return Err(format!("scope `{other}` is not one of once, session, until_revoked, so nothing was decided")),
        };
        let decided = match (p.outcome.as_deref(), p.allow) {
            (Some("allow"), _) | (None, Some(true)) => Decided::allowed(scope),
            (Some("reject"), _) | (None, Some(false)) => Decided::rejected(),
            (Some("cancel"), _) => Decided::cancelled(),
            (Some(other), _) => return Err(format!("outcome `{other}` is not one of allow, reject, cancel, so nothing was decided")),
            (None, None) => {
                return Err(
                    "the answer names no decision, so nothing was decided. Send `allow: true|false` or `outcome: allow|reject|cancel`"
                        .into(),
                );
            }
        };
        let pending = {
            let mut st = self.st();
            match st.approvals.remove(&p.id) {
                Some(a) if a.conn == conn => a,
                Some(a) => {
                    st.approvals.insert(p.id, a);
                    return Err(format!("approval {} was sent to another client; only that client can answer it", p.id));
                }
                None => {
                    return Err(format!("approval {} is not waiting (it was answered, or its turn ended), so nothing was decided", p.id));
                }
            }
        };
        pending
            .reply
            .send(decided)
            .map_err(|_| format!("approval {} is no longer waiting: its turn ended or was aborted, so nothing was decided", p.id))?;
        Ok(json!({ "id": p.id, "session": pending.session }))
    }

    async fn answer_preset(&self, conn: ConnId, id: u64) -> Result<Value, String> {
        let (pending, session, preset) = {
            let mut st = self.st();
            let pending = st
                .approvals
                .get(&id)
                .filter(|p| p.conn == conn && !p.reply.is_closed())
                .ok_or_else(|| format!("approval {id} is not waiting for this client; nothing was changed"))?;
            Daemon::owner_check(&st, conn, pending.session)?;
            let session = st.sessions.get(&pending.session).ok_or_else(|| not_open(pending.session))?.session.clone();
            let preset = pending.preset.clone().ok_or_else(|| format!("approval {id} names no preset cut; allow, reject or cancel it"))?;
            let pending = st.approvals.remove(&id).ok_or_else(|| format!("approval {id} is no longer waiting"))?;
            (pending, session, preset)
        };
        let value = match self.host.approval_preset(&session, &preset).await {
            Ok(value) => value,
            Err(error) => {
                if !pending.reply.is_closed() {
                    self.st().approvals.insert(id, pending);
                }
                return Err(error);
            }
        };
        pending
            .reply
            .send(Decided::allowed(GrantScope::Session))
            .map_err(|_| format!("the preset was recorded, but approval {id}'s turn ended before it could be answered"))?;
        Ok(value)
    }
}
