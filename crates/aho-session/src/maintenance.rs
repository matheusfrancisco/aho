//! Compaction, hooks, checkpoints & rewind, operator commands, resume.
//! Split out of the session driver (file-size ratchet, doc 12); child modules share the root's
//! private state and items crossing modules are `pub(crate)`.

use super::*;

impl Session {
    // ───────────────────────────── compaction ─────────────────────────────

    /// Pre-step pressure or provider overflow → summarize the oldest range under policy Nothing (10).
    pub(crate) async fn maybe_compact(&self, forced: bool) {
        if *lk(&self.compaction_failures) >= 3 {
            return;
        }
        let Ok(state) = self.fold() else { return };
        let messages = aho_log::derive_messages(&state);
        let chars: usize = serde_json::to_string(&messages).map(|s| s.len()).unwrap_or(0);
        let estimated = (chars / 4) as u64;
        let limit = (self.cfg.context_window as f64 * self.cfg.compaction_threshold) as u64;
        if !forced && estimated < limit {
            return;
        }
        let Some((start, end)) = compaction_range(&state, (self.cfg.context_window as f64 * self.cfg.compaction_retain * 4.0) as usize)
        else {
            return;
        };
        let id = Timestamp::now().0;
        self.append_reported(Principal::system("compaction"), EntryKind::CompactionStart { id });
        let window: Vec<Message> = aho_log::derive_messages(&State {
            items: state.items.iter().filter(|(i, _)| *i >= start && *i <= end).cloned().collect(),
            ..State::default()
        });
        let req = Request {
            model: self.cfg.model.clone(),
            system: vec![("compaction".into(), COMPACTION_PROMPT.into(), false)],
            messages: {
                let mut m = window;
                m.push(Message::user("Summarize the conversation above now, following the instructions."));
                m
            },
            tools: vec![],
            max_tokens: 4096,
            cache_key: None,
        };
        let result = self.llm.stream(req, Box::new(|_| {})).await;
        match result {
            Ok(r) => {
                let summary: String = r.content.iter().filter_map(|b| b.as_text()).collect();
                let shadowed =
                    state.items.iter().filter(|(i, _)| *i >= start && *i <= end).map(|(_, it)| format!("{it:?}").len()).sum::<usize>();
                if summary.len() >= shadowed || summary.trim().is_empty() {
                    *lk(&self.compaction_failures) += 1;
                    self.append_reported(
                        Principal::system("compaction"),
                        EntryKind::CompactionEnd { id, error: Some("summary did not shrink the range".into()) },
                    );
                    return;
                }
                self.append_reported(
                    Principal::system("compaction"),
                    EntryKind::SurfaceReplace {
                        start,
                        end,
                        shadow_price: (shadowed - summary.len()) as u64,
                        summary: Some(summary),
                        by_cap: "compaction".into(),
                    },
                );
                *lk(&self.compaction_failures) = 0;
                self.append_reported(Principal::system("compaction"), EntryKind::CompactionEnd { id, error: None });
            }
            Err(e) => {
                let n = {
                    let mut g = lk(&self.compaction_failures);
                    *g += 1;
                    *g
                };
                self.append_reported(Principal::system("compaction"), EntryKind::CompactionEnd { id, error: Some(e.to_string()) });
                self.append_reported(Principal::system("compaction"), EntryKind::CompactionFailed { consecutive: n });
            }
        }
    }

    // ───────────────────────────── hooks ─────────────────────────────

    /// Runs every hook registered for `event`. A timed-out hook yields its `on_error` decision
    /// (default Deny, 12). Returns the combined outcome; hooks only narrow (`⊓`).
    pub(crate) async fn run_hooks(&self, event: HookEvent, payload: Value) -> HookVerdict {
        let mut verdict = HookVerdict::default();
        for (cap, h) in &self.reg.hooks {
            if !h.on.contains(&event) {
                continue;
            }
            self.append_reported(
                Principal::system(cap.name),
                EntryKind::HookInvoked { point: format!("{event:?}"), cap: cap.name.into(), matcher: String::new() },
            );
            let started = std::time::Instant::now();
            let out = tokio::time::timeout(Duration::from_millis(h.timeout_ms.max(1)), (h.run)(event, &payload)).await;
            let (decision, ctx) = match out {
                Ok(o) => {
                    if let Some(aho_cap::Decision::Deny(r)) = &o.decision {
                        verdict.deny.get_or_insert_with(|| r.clone());
                    }
                    if let Some(r) = &o.stop {
                        verdict.stop.get_or_insert_with(|| r.clone());
                    }
                    (o.decision.map(|d| format!("{d:?}")).unwrap_or_else(|| "none".into()), o.context)
                }
                Err(_) => {
                    if h.on_error == aho_cap::OnError::Deny {
                        verdict.deny.get_or_insert_with(|| format!("hook from {} timed out", cap.name));
                    }
                    (format!("timeout → {:?}", h.on_error), vec![])
                }
            };
            self.append_reported(
                Principal::system(cap.name),
                EntryKind::HookResult { decision, exit: None, stderr_summary: String::new(), ms: started.elapsed().as_millis() as u64 },
            );
            for c in ctx {
                self.append_reported(Principal::system(cap.name), EntryKind::UserMessage { form: Form::Notice, text: c, client_key: None });
            }
        }
        verdict
    }

    // ───────────────────────────── checkpoints & rewind (11) ─────────────────────────────

    pub(crate) async fn checkpoint(&self, cause: CheckpointCause) -> Option<CheckpointId> {
        let snap = self.reg.handle::<SnapshotHandle>()?;
        let tree = snap.0.tree().await.ok()?;
        let state = self.fold().ok()?;
        if state.checkpoints.last().map(|c| c.1) == Some(tree) && !matches!(cause, CheckpointCause::PreRewind | CheckpointCause::Manual) {
            return None;
        }
        let id = CheckpointId(state.checkpoints.len() as u64 + 1);
        let parent = state.checkpoints.last().map(|c| c.0);
        self.append(Principal::system("checkpoint"), EntryKind::WorkspaceCheckpoint { id, tree, parent, cause }).ok()?;
        Some(id)
    }

    pub async fn checkpoint_now(&self) -> Option<CheckpointId> {
        self.checkpoint(CheckpointCause::Manual).await
    }

    /// PreRewind checkpoint first, so the rewind is itself reversible; then fork and/or restore.
    pub async fn rewind(&self, _op: &Operator, to: CheckpointId, mode: RewindMode) -> Result<EntryId, SessionError> {
        let state = self.fold()?;
        let (_, tree, at) = state
            .checkpoints
            .iter()
            .find(|c| c.0 == to)
            .copied()
            .ok_or_else(|| SessionError::NotFound(format!("no checkpoint c{}", to.0)))?;
        for (op, _, _) in self.ops.running() {
            self.ops.cancel(op);
        }
        let pre = self.checkpoint(CheckpointCause::PreRewind).await.unwrap_or(to);
        let (mut changed, mut deleted) = (vec![], vec![]);
        if mode != RewindMode::ChatOnly {
            let snap = self
                .reg
                .handle::<SnapshotHandle>()
                .ok_or_else(|| SessionError::NotFound("this world has no Snapshot provider; rewind of code is Locked".into()))?;
            let d = snap.0.restore(tree).await.map_err(|e| SessionError::Boot(e.to_string()))?;
            changed = d.changed;
            deleted = d.deleted;
        }
        if mode != RewindMode::CodeOnly {
            self.log.fork(at)?;
            self.close_branch(CancelReason::Rewind)?;
        }
        let e = self.append(
            Principal::Operator,
            EntryKind::WorkspaceRestored { from: pre, to, mode, changed: changed.clone(), deleted: deleted.clone() },
        )?;
        if mode == RewindMode::CodeOnly {
            self.append(Principal::system("checkpoint"), EntryKind::UserMessage {
                form: Form::Notice,
                text: format!("Workspace restored to checkpoint c{}; files changed: {}; deleted: {}. Earlier edits in this conversation are no longer on disk.", to.0, changed.join(", "), deleted.join(", ")),
                client_key: None,
            })?;
        }
        Ok(e.id)
    }

    /// After a fork into the middle of a turn: nothing may stay open on the new branch (06, 11).
    /// Pending approvals are cancelled, open calls get a synthetic cancelled result, open ops are
    /// closed, and the open turn is ended — so the next turn folds and never waits on ghosts.
    pub(crate) fn close_branch(&self, reason: CancelReason) -> Result<(), SessionError> {
        let state = self.fold()?;
        for req in state.pending_approvals.keys().copied() {
            self.append(
                Principal::system("rewind"),
                EntryKind::ApprovalDecided { req, outcome: ApprovalOutcome::Cancelled, scope: GrantScope::Once },
            )?;
        }
        for call in &state.open_calls {
            self.append(
                Principal::system("rewind"),
                EntryKind::ToolResult {
                    call: call.clone(),
                    outcome: ToolOutcome::Cancelled { reason },
                    value: Value::Null,
                    content: vec![ContentBlock::text(
                        "Error: this call was cancelled because the conversation was rewound before it finished",
                    )],
                    spill_ref: None,
                    meta: None,
                    late: false,
                },
            )?;
        }
        for op in state.open_ops.keys().copied() {
            self.append(Principal::system("rewind"), EntryKind::OpDone { op, status: OpStatus::Cancelled { reason } })?;
        }
        if let Some(turn) = state.turn {
            self.append(
                Principal::system("rewind"),
                EntryKind::StepEnd { step: StepId(0), transition: Transition::Aborted("rewound".into()) },
            )?;
            self.append(Principal::system("rewind"), EntryKind::TurnEnd { turn })?;
        }
        Ok(())
    }

    // ───────────────────────────── operator & resume ─────────────────────────────

    /// `aho shell -- <cmd>`: an operator command in the same world, logged and shown to the agent.
    pub async fn operator_command(&self, _op: &Operator, command: &str) -> Result<String, SessionError> {
        let sp = self.reg.handle::<SubprocessHandle>().ok_or_else(|| SessionError::NotFound("no Subprocess provider is bound".into()))?;
        // One view, one lock: a second `lk` inside the same expression would wait on the guard
        // the first one still holds (temporaries live to the end of the statement).
        let view = lk(&self.policy).view();
        if let Some((aho_policy::SafetyKind::Catastrophic, why)) =
            aho_policy::safety_check("bash", &serde_json::json!({ "command": command }), &view.workspace, &view.home)
        {
            return Err(SessionError::Boot(format!("refused: {why}")));
        }
        let call = CallId(format!("op-{}", Timestamp::now().0));
        self.append(
            Principal::Operator,
            EntryKind::ToolCall {
                call: call.clone(),
                tool: "shell".into(),
                args: serde_json::json!({ "command": command }),
                class: Consequence::SpawnsProcess,
            },
        )?;
        let cwd = view.workspace;
        let child =
            sp.0.spawn(SpawnSpec {
                argv: vec!["/bin/sh".into(), "-c".into(), command.into()],
                cwd: Some(cwd),
                env: vec![],
                stdin: None,
                timeout_ms: None,
            })
            .await
            .map_err(|e| SessionError::Boot(format!("{e:?}")))?;
        let exit = child.wait().await.map_err(|e| SessionError::Boot(format!("{e:?}")))?;
        let (out, _, _) = child.output(0);
        let text = String::from_utf8_lossy(&out).to_string();
        self.append_result_now(&call, ExecOutcome::text(format!("{text}\n[exit {}]", exit.code)))?;
        self.append(
            Principal::system("session"),
            EntryKind::UserMessage { form: Form::Notice, text: format!("Operator ran `{command}`; exit {}.", exit.code), client_key: None },
        )?;
        Ok(text)
    }

    /// Ops that were open at load: re-runnable → re-dispatch; external effect → Uncertain; else Failed (16).
    pub(crate) async fn recover_open_ops(self: &Arc<Self>, state: &State) -> Result<(), SessionError> {
        for (op, (call, rerunnable, external)) in &state.open_ops {
            if *rerunnable
                && let (Some((_, _, args)), Some((decl, name, exec))) =
                    (state.calls.get(call), state.calls.get(call).and_then(|c| self.lookup(&c.0)))
            {
                self.append(
                    Principal::system("recovery"),
                    EntryKind::OpDone { op: *op, status: OpStatus::Failed { err: "re-dispatched after restart".into() } },
                )?;
                let s = self.spawn_exec(decl, name, exec, call.clone(), args.clone(), Principal::system("recovery"), vec![]);
                self.log_op_started(&s, decl)?;
                continue;
            }
            let status = if *external {
                OpStatus::Uncertain { last_seen: Timestamp::now() }
            } else {
                OpStatus::Failed { err: "interrupted by a restart".into() }
            };
            self.append(Principal::system("recovery"), EntryKind::OpDone { op: *op, status: status.clone() })?;
            if state.open_calls.contains(call) {
                let reason = if *external {
                    "The harness restarted while this call was running. It has an external effect and may or may not have happened; check its effect before repeating it."
                } else {
                    "The harness restarted while this call was running; it did not complete. Workspace changes it made are covered by the last checkpoint."
                };
                self.append_result_now(call, ExecOutcome::err(if *external { "UNCERTAIN" } else { "INTERRUPTED" }, reason))?;
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub(crate) struct HookVerdict {
    pub(crate) deny: Option<String>,
    pub(crate) stop: Option<String>,
}

/// Oldest replaceable range that never splits a call/result pair, leaving a verbatim tail of `retain_chars`.
pub fn compaction_range(state: &State, retain_chars: usize) -> Option<(EntryId, EntryId)> {
    let skipped: BTreeSet<EntryId> =
        state.items.iter().filter(|(i, _)| state.replaces.iter().any(|(a, b, _)| i >= a && i <= b)).map(|(i, _)| *i).collect();
    let live: Vec<&(EntryId, aho_log::Item)> = state.items.iter().filter(|(i, _)| !skipped.contains(i)).collect();
    if live.len() < 4 {
        return None;
    }
    let sizes: Vec<usize> = live.iter().map(|(_, it)| format!("{it:?}").len()).collect();
    let mut tail = 0usize;
    let mut cut = live.len();
    for (i, s) in sizes.iter().enumerate().rev() {
        tail += s;
        if tail >= retain_chars {
            cut = i;
            break;
        }
    }
    // walk back until no call is open at the boundary
    let mut end_idx = None;
    let mut open: BTreeSet<CallId> = BTreeSet::new();
    for (i, (_, it)) in live.iter().enumerate() {
        match it {
            aho_log::Item::Assistant { content } => {
                for b in content {
                    if let ContentBlock::ToolUse { id, .. } = b {
                        open.insert(id.clone());
                    }
                }
            }
            aho_log::Item::Result { call, .. } => {
                open.remove(call);
            }
            _ => {}
        }
        if i + 1 < cut && open.is_empty() && !matches!(it, aho_log::Item::Boundary) {
            end_idx = Some(i);
        }
    }
    let end_idx = end_idx?;
    let first = live.iter().position(|(_, it)| !matches!(it, aho_log::Item::Boundary))?;
    if end_idx <= first {
        return None;
    }
    // nothing worth summarizing unless the range holds at least one assistant turn
    if !live[first..=end_idx].iter().any(|(_, it)| matches!(it, aho_log::Item::Assistant { .. })) {
        return None;
    }
    Some((live[first].0, live[end_idx].0))
}

pub const COMPACTION_PROMPT: &str = "You are compacting a coding session. Write a dense summary of the conversation so far that preserves: the user's goals and constraints, decisions made and why, files read or changed (paths), commands run and their outcomes, errors still open, and what was about to happen next. Use plain prose and short lists. Do not call tools.";
