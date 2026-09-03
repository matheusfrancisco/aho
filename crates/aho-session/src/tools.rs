//! Tool calls: lookup, review, approvals, execution, results, spill.
//! Split out of the session driver (file-size ratchet, doc 12); child modules share the root's
//! private state and items crossing modules are `pub(crate)`.

use super::*;

impl Session {
    // ───────────────────────────── tool calls ─────────────────────────────

    pub(crate) fn view(&self, call: &CallId, name: &str, class: Consequence, args: &Value, by: Principal) -> ExecView {
        ExecView { call: call.clone(), tool: name.to_string(), class, args: args.clone(), policy: lk(&self.policy).view(), by }
    }

    /// Static tools by name or alias, then runtime-registered (MCP) tools backed by a class template.
    pub(crate) fn lookup(&self, name: &str) -> Option<(&'static ToolDecl, String, ToolExec)> {
        if let Some((d, e)) = self.reg.tool(name) {
            return Some((d, d.name.to_string(), e));
        }
        self.reg
            .dynamic_tools
            .iter()
            .find(|(_, s, _)| s.name == name)
            .map(|(_, s, e)| (&DYNAMIC[s.class as usize], s.name.clone(), e.clone()))
    }

    pub(crate) fn cap_of(&self, tool: &str) -> Option<String> {
        self.reg
            .tools
            .iter()
            .find(|(_, d, _)| d.name == tool || d.aliases.contains(&tool))
            .map(|(c, _, _)| c.name.to_string())
            .or_else(|| self.reg.dynamic_tools.iter().find(|(_, s, _)| s.name == tool).map(|(c, _, _)| c.name.to_string()))
    }

    pub(crate) fn review(&self, decl: &ToolDecl, name: &str, call: &CallId, args: &Value, by: &Principal) -> ChainOutcome {
        let view = self.view(call, name, decl.class, args, by.clone());
        let guard_denials: Vec<(String, String)> =
            self.reg.guards.iter().filter_map(|(cap, g)| g(&view).map(|r| (cap.name.to_string(), r))).collect();
        let escalate: Vec<Consequence> = args
            .get("escalate")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).filter_map(Consequence::parse).collect())
            .unwrap_or_default();
        let mut policy = lk(&self.policy).clone();
        if self.allowed_caps.iter().any(|c| self.cap_of(name).as_deref() == Some(c.as_str())) {
            // `aho allow <cap>`: operator config lifts the preset ceiling for this capability's calls
            // only; deny rules, guards and safety checks (steps 1–5) still run first.
            policy = SandboxPolicy::new(
                policy.allowed().with(decl.class),
                policy.world().clone(),
                policy.workspace().to_string(),
                policy.home().to_string(),
                policy.epoch(),
            );
        }
        aho_policy::decide(&CallUnderReview {
            decl,
            args,
            by,
            rules: &self.rules,
            policy: &policy,
            guard_denials,
            escalate,
            tool_name: Some(name),
        })
    }

    pub(crate) fn try_early_start(self: &Arc<Self>, turn: TurnId, call: &CallId, name: &str, args: &Value) -> Option<Started> {
        let (decl, name, exec) = self.lookup(name)?;
        if (decl.concurrency)(args) != aho_cap::Concurrency::Safe {
            return None;
        }
        let by = Principal::Agent { id: AgentId(0), turn };
        if !matches!(self.review(decl, &name, call, args, &by), ChainOutcome::Allow(_)) {
            return None;
        }
        if self.pre_execute(&self.view(call, &name, decl.class, args, by.clone())) != PreDecision::Allow {
            return None;
        }
        Some(self.spawn_exec(decl, name, exec, call.clone(), args.clone(), by, vec![]))
    }

    pub(crate) fn pre_execute(&self, view: &ExecView) -> PreDecision {
        for (_, f) in &self.reg.pre {
            match f(view) {
                PreDecision::Allow => continue,
                other => return other,
            }
        }
        PreDecision::Allow
    }

    pub(crate) async fn dispatch_calls(
        self: &Arc<Self>,
        turn: TurnId,
        uses: Vec<(CallId, String, Value)>,
        mut early: BTreeMap<CallId, Started>,
    ) -> Result<(), SessionError> {
        let by = Principal::Agent { id: AgentId(0), turn };
        let mut barrier: Vec<watch::Receiver<bool>> = Vec::new();
        let mut since_barrier: Vec<watch::Receiver<bool>> = Vec::new();
        for (call, name, args) in uses {
            let Some((decl, name, exec)) = self.lookup(&name) else {
                self.append(
                    by.clone(),
                    EntryKind::ToolCall { call: call.clone(), tool: name.clone(), args: args.clone(), class: Consequence::ReadOnly },
                )?;
                let why = self.why_missing(&name);
                self.append_result_now(&call, ExecOutcome::err("UNKNOWN_TOOL", why))?;
                continue;
            };
            self.append(by.clone(), EntryKind::ToolCall { call: call.clone(), tool: name.clone(), args: args.clone(), class: decl.class })?;
            if let Some(s) = early.remove(&call) {
                self.log_op_started(&s, decl)?;
                since_barrier.push(s.done);
                continue;
            }
            let outcome = self.review(decl, &name, &call, &args, &by);
            let allowed = match &outcome {
                ChainOutcome::Allow(_) => true,
                ChainOutcome::Ask(_) => self.ask(decl, &name, &call, &args, &outcome).await?,
                ChainOutcome::Deny(_) | ChainOutcome::DenyEscalatable { .. } => {
                    lk(&self.denials).record(true);
                    self.append_result_now(&call, ExecOutcome::err("DENIED", aho_policy::render(&outcome, &name)))?;
                    false
                }
            };
            if !allowed {
                continue;
            }
            let view = self.view(&call, &name, decl.class, &args, by.clone());
            match self.pre_execute(&view) {
                PreDecision::Allow => {}
                PreDecision::Deny { reason } => {
                    self.append_result_now(&call, ExecOutcome::err("DENIED_BY_HOOK", reason))?;
                    continue;
                }
                PreDecision::Cancel => {
                    self.append_result_now(&call, ExecOutcome::Cancelled)?;
                    continue;
                }
                PreDecision::Ask { reason } => {
                    let o = ChainOutcome::Ask(aho_policy::DecisionReason::Hook(reason.unwrap_or_default()));
                    if !self.ask(decl, &name, &call, &args, &o).await? {
                        continue;
                    }
                }
            }
            lk(&self.denials).record(false);
            let exclusive = (decl.concurrency)(&args) == aho_cap::Concurrency::Exclusive;
            let deps: Vec<watch::Receiver<bool>> =
                if exclusive { barrier.iter().chain(since_barrier.iter()).cloned().collect() } else { barrier.clone() };
            let s = self.spawn_exec(decl, name, exec, call.clone(), args.clone(), by.clone(), deps);
            self.log_op_started(&s, decl)?;
            if exclusive {
                barrier = vec![s.done];
                since_barrier.clear();
            } else {
                since_barrier.push(s.done);
            }
        }
        Ok(())
    }

    pub(crate) fn why_missing(&self, name: &str) -> String {
        format!(
            "There is no active tool named `{name}` in this session. Call `aho_why` with the name to see whether it is locked or disabled and what would enable it."
        )
    }

    /// Approval flow; both entries sit inside the open turn (04). A refusal is the call's result.
    pub(crate) async fn ask(
        &self,
        decl: &ToolDecl,
        name: &str,
        call: &CallId,
        args: &Value,
        outcome: &ChainOutcome,
    ) -> Result<bool, SessionError> {
        match self.ask_operator(decl, name, call, args, outcome).await? {
            None => Ok(true),
            Some(refused) => {
                self.append_result_now(call, ExecOutcome::err("APPROVAL", aho_policy::render_outcome(refused, name)))?;
                Ok(false)
            }
        }
    }

    /// The approval round trip: `None` = allowed (a non-once grant widens the policy), else the
    /// refusing outcome; nothing is appended for `call` itself.
    pub(crate) async fn ask_operator(
        &self,
        decl: &ToolDecl,
        name: &str,
        call: &CallId,
        args: &Value,
        outcome: &ChainOutcome,
    ) -> Result<Option<ApprovalOutcome>, SessionError> {
        // Class-aware card: carries the smallest preset/allow cut that would pass this call (05).
        let policy = lk(&self.policy).clone();
        let mut card = aho_policy::ApprovalCard::new(decl, args, outcome, &policy);
        if card.tool != name {
            // runtime-registered tools share a class template; name the real tool
            card.tool = name.to_string();
            card.rendered = aho_policy::render(outcome, name);
        }
        let req = self.append(
            Principal::system("policy"),
            EntryKind::ApprovalRequested {
                wants: ConsequenceSet::EMPTY.with(decl.class),
                for_cap: name.to_string(),
                call: Some(call.clone()),
                reason: card.rendered.clone(),
            },
        )?;
        let decided = self.approval.request(card).await;
        self.append(Principal::Operator, EntryKind::ApprovalDecided { req: req.id, outcome: decided.outcome, scope: decided.scope })?;
        if decided.outcome == ApprovalOutcome::AllowedOnce {
            // A session grant widens only when the class is not already allowed (an operator may
            // have switched to a preset that covers it while the card waited). The label is derived,
            // so a widening that lands exactly on a preset keeps its name.
            let mut p = lk(&self.policy);
            if decided.scope != GrantScope::Once && !p.allowed().contains(decl.class) {
                let widened = p.widen_by_approval(&self.operator, req.id, p.allowed().with(decl.class));
                self.append(
                    Principal::Operator,
                    EntryKind::PolicyEpoch {
                        epoch: widened.epoch(),
                        allowed: widened.allowed(),
                        preset: widened.preset().name().into(),
                        approval_ref: Some(req.id),
                    },
                )?;
                *p = widened;
            }
            Ok(None)
        } else {
            lk(&self.denials).record(true);
            Ok(Some(decided.outcome))
        }
    }

    /// PTC (10): one tool call made by a running `run_code` program — the same lookup, decision
    /// chain, approvals and pre/post hooks as a model call, under the parent call's principal.
    /// Not logged as `Tool/Call` (the model never made it; `derive_messages` would show it): the
    /// program's tool records `Ptc/Call` + `Ptc/Result`. Approvals name the parent call. Post-hook
    /// deferred context is not delivered: it would land between the parent's call and its result.
    pub(crate) async fn nested_call(self: &Arc<Self>, parent: ExecView, cancel: CancelToken, tool: String, args: Value) -> ExecOutcome {
        let Some((decl, name, exec)) = self.lookup(&tool) else {
            return ExecOutcome::err("UNKNOWN_TOOL", self.why_missing(&tool));
        };
        if name == parent.tool {
            return ExecOutcome::err("PTC_RECURSION", format!("`{name}` cannot call itself from inside a program; the call did not run."));
        }
        let n = self.ptc_seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let call = CallId(format!("{}.ptc{n}", parent.call.0));
        let by = parent.by.clone();
        let chain = self.review(decl, &name, &call, &args, &by);
        let chain_ask = match &chain {
            ChainOutcome::Allow(_) => None,
            ChainOutcome::Ask(_) => Some(chain.clone()),
            ChainOutcome::Deny(_) | ChainOutcome::DenyEscalatable { .. } => {
                lk(&self.denials).record(true);
                return ExecOutcome::err("DENIED", aho_policy::render(&chain, &name));
            }
        };
        if let Some(o) = chain_ask
            && let Some(refused) = self.nested_approval(decl, &name, &parent.call, &args, &o).await
        {
            return refused;
        }
        let view = self.view(&call, &name, decl.class, &args, by);
        match self.pre_execute(&view) {
            PreDecision::Allow => {}
            PreDecision::Deny { reason } => return ExecOutcome::err("DENIED_BY_HOOK", reason),
            PreDecision::Cancel => return ExecOutcome::Cancelled,
            PreDecision::Ask { reason } => {
                let o = ChainOutcome::Ask(aho_policy::DecisionReason::Hook(reason.unwrap_or_default()));
                if let Some(refused) = self.nested_approval(decl, &name, &parent.call, &args, &o).await {
                    return refused;
                }
            }
        }
        lk(&self.denials).record(false);
        let mut outcome = exec(ExecRequest { view: view.clone(), cancel, progress: None }).await;
        for (_, f) in &self.reg.post {
            match f(&view, &outcome) {
                PostDecision::Accept { content, .. } => {
                    if let (Some(c), ExecOutcome::Done { content: old, .. }) = (content, &mut outcome) {
                        *old = c;
                    }
                }
                PostDecision::Block { feedback, .. } => outcome = ExecOutcome::err("BLOCKED_BY_HOOK", feedback),
            }
        }
        outcome
    }

    /// `None` = approved; else the outcome the program gets instead of running the call.
    async fn nested_approval(&self, decl: &ToolDecl, name: &str, parent: &CallId, args: &Value, o: &ChainOutcome) -> Option<ExecOutcome> {
        match self.ask_operator(decl, name, parent, args, o).await {
            Ok(None) => None,
            Ok(Some(refused)) => Some(ExecOutcome::err("APPROVAL", aho_policy::render_outcome(refused, name))),
            Err(e) => Some(ExecOutcome::err("SESSION_LOG", format!("the approval could not be logged ({e}); the call did not run."))),
        }
    }

    #[allow(clippy::too_many_arguments)] // one call site per dispatch path; a struct would only rename the list
    pub(crate) fn spawn_exec(
        self: &Arc<Self>,
        decl: &'static ToolDecl,
        name: String,
        exec: ToolExec,
        call: CallId,
        args: Value,
        by: Principal,
        deps: Vec<watch::Receiver<bool>>,
    ) -> Started {
        let (op, cancel) = self.ops.register(call.clone(), &name, decl.op_kind, decl.rerunnable, decl.external_effect);
        let turn_token = lk(&self.turn_cancel).clone();
        let (done_tx, done_rx) = watch::channel(false);
        let me = self.clone();
        let sem = limiter(self.cfg.max_parallel);
        let view = self.view(&call, &name, decl.class, &args, by);
        let progress = aho_cap::tokio_like::Sender::default();
        let ops = self.ops.clone();
        let call2 = call.clone();
        tokio::spawn(async move {
            for mut d in deps {
                let _ = d.wait_for(|x| *x).await;
            }
            let _permit = sem.acquire_owned().await;
            let outcome = if cancel.is_cancelled() || turn_token.is_cancelled() {
                ExecOutcome::Cancelled
            } else {
                let child = cancel.clone();
                let tt = turn_token.clone();
                let fut = exec(ExecRequest { view: view.clone(), cancel: child.clone(), progress: Some(progress.clone()) });
                tokio::pin!(fut);
                loop {
                    tokio::select! {
                        o = &mut fut => break o,
                        _ = tokio::time::sleep(Duration::from_millis(50)) => {
                            for line in progress.drain() { ops.progress(op, line); }
                            if tt.is_cancelled() && !child.is_cancelled() { child.cancel(); }
                        }
                    }
                }
            };
            let mut post_context = Vec::new();
            let mut outcome = outcome;
            for (_, f) in &me.reg.post {
                match f(&view, &outcome) {
                    PostDecision::Accept { content, deferred_context } => {
                        if let (Some(c), ExecOutcome::Done { content: old, .. }) = (content, &mut outcome) {
                            *old = c;
                        }
                        post_context.extend(deferred_context);
                    }
                    PostDecision::Block { feedback, deferred_context } => {
                        outcome = ExecOutcome::err("BLOCKED_BY_HOOK", feedback);
                        post_context.extend(deferred_context);
                    }
                }
            }
            let _ = me.results_tx.send(Finished { call: call2, op, outcome, post_context });
            let _ = done_tx.send(true);
        });
        Started { op, done: done_rx }
    }

    pub(crate) fn log_op_started(&self, s: &Started, decl: &ToolDecl) -> Result<(), SessionError> {
        let call = self.ops.list().into_iter().find(|i| i.op == s.op).map(|i| i.call).unwrap_or(CallId(String::new()));
        self.append(
            Principal::system("ops"),
            EntryKind::OpStarted { op: s.op, call, kind: decl.op_kind, rerunnable: decl.rerunnable, external_effect: decl.external_effect },
        )?;
        Ok(())
    }

    /// Urgent input (point C): calls not yet started are skipped with a synthetic result.
    /// Urgent input (point C): calls whose ops have not started are cancelled; each yields a
    /// synthetic `Cancelled` result when its task observes the token.
    pub(crate) fn skip_unstarted(&self) {
        for info in self.ops.list() {
            if info.status.is_none() && info.identity.is_none() {
                self.ops.cancel(info.op);
            }
        }
    }

    pub(crate) fn append_result_now(&self, call: &CallId, outcome: ExecOutcome) -> Result<(), SessionError> {
        let (tool_outcome, value, content, meta) = split_outcome(outcome);
        self.append(
            Principal::system("tool"),
            EntryKind::ToolResult { call: call.clone(), outcome: tool_outcome, value, content, spill_ref: None, meta, late: false },
        )?;
        Ok(())
    }

    pub(crate) async fn record_result(&self, f: Finished) -> Result<(), SessionError> {
        let state = self.fold()?;
        let (tool, class) = state.calls.get(&f.call).map(|c| (c.0.clone(), c.1)).unwrap_or_default_pair();
        let late = state.last_request_after_call(&f.call);
        let decl = self.lookup(&tool).map(|(d, _, _)| d);
        let (outcome, value, content, meta) = split_outcome(f.outcome.clone());
        let (content, spill_ref) = self.spill(&tool, content, decl.and_then(|d| d.max_inline));
        let status = match &f.outcome {
            ExecOutcome::Done { .. } => OpStatus::Completed { code: 0 },
            ExecOutcome::Err { reason, .. } => OpStatus::Failed { err: reason.clone() },
            ExecOutcome::Cancelled => OpStatus::Cancelled { reason: CancelReason::Aborted },
        };
        self.ops.finish(f.op, status.clone());
        if state.open_calls.contains(&f.call) {
            let events = tool_events(&value);
            self.append(
                Principal::system("tool"),
                EntryKind::ToolResult { call: f.call.clone(), outcome, value, content, spill_ref, meta, late },
            )?;
            for kind in events {
                self.append(Principal::system(&tool), kind)?;
            }
        }
        if state.open_ops.contains_key(&f.op) {
            self.append(Principal::system("ops"), EntryKind::OpDone { op: f.op, status })?;
        }
        for c in f.post_context {
            self.append(Principal::system("post-execute"), EntryKind::UserMessage { form: Form::Notice, text: c, client_key: None })?;
        }
        self.run_hooks(HookEvent::ToolPost, serde_json::json!({ "tool": tool, "call": f.call.0 })).await;
        if self.cfg.checkpoint_per_call
            && matches!(class, Consequence::WorkspaceWrite | Consequence::SpawnsProcess | Consequence::HostWrite)
        {
            self.checkpoint(CheckpointCause::ToolResult(f.call)).await;
        }
        Ok(())
    }

    /// Spill, don't truncate (10): head + [...] + tail + notice with the locator.
    #[allow(clippy::disallowed_methods, reason = "authority: session-spill")]
    pub(crate) fn spill(&self, tool: &str, content: Vec<ContentBlock>, max_inline: Option<usize>) -> (Vec<ContentBlock>, Option<Hash>) {
        let limit = match max_inline {
            None => return (content, None),
            Some(n) => n.min(self.cfg.spill_budget),
        };
        let text: String = content.iter().filter_map(|b| b.as_text()).collect::<Vec<_>>().join("\n");
        if text.chars().count() <= limit {
            return (content, None);
        }
        let hash = Hash::of(text.as_bytes());
        let path = self.cfg.spill_dir.join(format!("{}.txt", hash.to_hex()));
        if let Err(e) = std::fs::create_dir_all(&self.cfg.spill_dir).and_then(|_| std::fs::write(&path, &text)) {
            let _ = self.events.send(SessionEvent::Error(format!("spill failed ({e}); keeping {tool} output inline")));
            return (content, None);
        }
        let half = limit / 2;
        let head: String = text.chars().take(half).collect();
        let tail: String = {
            let v: Vec<char> = text.chars().collect();
            v[v.len().saturating_sub(half)..].iter().collect()
        };
        let omitted = text.len().saturating_sub(head.len() + tail.len());
        let mut blocks: Vec<ContentBlock> = content.into_iter().filter(|b| b.as_text().is_none()).collect();
        blocks.insert(0, ContentBlock::text(format!(
            "{head}\n\n[...]\n\n{tail}\n[{tool} output truncated: {omitted} bytes omitted. Full output saved at {} (spill {}); read ranges of it with the read tool.]",
            path.display(),
            hash.short()
        )));
        (blocks, Some(hash))
    }
}

/// Class templates for runtime-registered tools (MCP): remote, not re-runnable, external effect
/// (crash-honest: a mid-dispatch MCP call folds to `Uncertain`). Indexed by `Consequence as usize`.
pub(crate) static DYNAMIC: [ToolDecl; 8] = [
    dyn_decl(Consequence::ReadOnly),
    dyn_decl(Consequence::WorkspaceWrite),
    dyn_decl(Consequence::HostWrite),
    dyn_decl(Consequence::NetworkEgress),
    dyn_decl(Consequence::SpawnsProcess),
    dyn_decl(Consequence::ControlsVM),
    dyn_decl(Consequence::ControlsDesktop),
    dyn_decl(Consequence::TalksToHuman),
];

pub(crate) const fn dyn_decl(c: Consequence) -> ToolDecl {
    ToolDecl::new("dynamic", "runtime-registered tool", c).op(aho_log::OpKind::Remote, false, true)
}

/// Approval channel handed to tools: logs `ApprovalRequested`/`ApprovalDecided` around the
/// operator's answer, inside the open turn; outside a turn it answers `Unavailable` without asking.
pub(crate) struct LoggedApproval {
    pub(crate) inner: Arc<dyn Approval>,
    pub(crate) log: Arc<dyn LogStore>,
}

impl Approval for LoggedApproval {
    fn policy(&self) -> AskPolicy {
        self.inner.policy()
    }
    fn request(&self, card: aho_policy::ApprovalCard) -> BoxFuture<'_, aho_approval::Decided> {
        Box::pin(async move {
            let unavailable = aho_approval::Decided { outcome: ApprovalOutcome::Unavailable, scope: GrantScope::Once };
            let open = fold_head(&*self.log, &CustomFolds::new()).map(|s| s.turn.is_some()).unwrap_or(false);
            if !open {
                return unavailable;
            }
            let Ok(req) = self.log.append(
                None,
                Principal::system("policy"),
                EntryKind::ApprovalRequested {
                    wants: ConsequenceSet::EMPTY.with(card.class),
                    for_cap: card.tool.clone(),
                    call: None,
                    reason: card.rendered.clone(),
                },
            ) else {
                return unavailable;
            };
            let decided = self.inner.request(card).await;
            // If the turn ended meanwhile the loop already cancelled this request; do not decide twice.
            let still_pending =
                fold_head(&*self.log, &CustomFolds::new()).map(|s| s.pending_approvals.contains_key(&req.id)).unwrap_or(false);
            if !still_pending {
                return aho_approval::Decided { outcome: ApprovalOutcome::Cancelled, scope: GrantScope::Once };
            }
            if self
                .log
                .append(
                    None,
                    Principal::Operator,
                    EntryKind::ApprovalDecided { req: req.id, outcome: decided.outcome, scope: decided.scope },
                )
                .is_err()
            {
                return unavailable;
            }
            decided
        })
    }
}

pub(crate) struct Started {
    op: OpId,
    done: watch::Receiver<bool>,
}

pub(crate) fn limiter(n: usize) -> Arc<Semaphore> {
    Arc::new(Semaphore::new(n.max(1)))
}

pub(crate) trait PairDefault {
    fn unwrap_or_default_pair(self) -> (String, Consequence);
}
impl PairDefault for Option<(String, Consequence)> {
    fn unwrap_or_default_pair(self) -> (String, Consequence) {
        self.unwrap_or((String::new(), Consequence::ReadOnly))
    }
}

pub(crate) trait LateCheck {
    fn last_request_after_call(&self, call: &CallId) -> bool;
}
impl LateCheck for State {
    /// A result is late when a request boundary happened after its call (16).
    fn last_request_after_call(&self, call: &CallId) -> bool {
        let call_pos = self.items.iter().position(|(_, it)| matches!(it, aho_log::Item::Assistant { content } if content.iter().any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if id == call))));
        match call_pos {
            Some(p) => self.items[p + 1..].iter().any(|(_, it)| matches!(it, aho_log::Item::Boundary)),
            None => false,
        }
    }
}

pub(crate) fn split_outcome(o: ExecOutcome) -> (ToolOutcome, Value, Vec<ContentBlock>, Option<Card>) {
    match o {
        ExecOutcome::Done { value, content, meta } => (ToolOutcome::Ok, value, content, meta),
        ExecOutcome::Err { code, reason } => (
            ToolOutcome::Err { code: code.clone(), reason: reason.clone() },
            Value::Null,
            vec![ContentBlock::text(format!("Error ({code}): {reason}"))],
            None,
        ),
        ExecOutcome::Cancelled => (
            ToolOutcome::Cancelled { reason: CancelReason::Aborted },
            Value::Null,
            vec![ContentBlock::text("Error: tool call aborted")],
            None,
        ),
    }
}

/// Log entries a tool may ask the session to append after its result (`value.events`).
/// Accepted shapes: `{"ns", "name", "body"}` → `Custom`; `{"kind": <EntryKind JSON>}` for the
/// whitelisted record kinds below. Core kinds (policy, approvals, turns, config) are never
/// accepted from a tool: authority does not flow through tool output (04).
pub fn tool_events(value: &Value) -> Vec<EntryKind> {
    let Some(arr) = value.get("events").and_then(|v| v.as_array()) else { return vec![] };
    arr.iter()
        .filter_map(|e| {
            if let (Some(ns), Some(name)) = (e.get("ns").and_then(|v| v.as_str()), e.get("name").and_then(|v| v.as_str())) {
                return Some(EntryKind::Custom {
                    ns: ns.to_string(),
                    name: name.to_string(),
                    body: e.get("body").cloned().unwrap_or(Value::Null),
                });
            }
            let kind: EntryKind = serde_json::from_value(e.get("kind")?.clone()).ok()?;
            matches!(
                kind,
                EntryKind::Custom { .. }
                    | EntryKind::SubagentStarted { .. }
                    | EntryKind::SubagentEnded { .. }
                    | EntryKind::InvariantRegistered { .. }
                    | EntryKind::InvariantChecked { .. }
                    | EntryKind::InvariantMutated { .. }
                    | EntryKind::Note { .. }
            )
            .then_some(kind)
        })
        .collect()
}

#[cfg(test)]
mod event_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tools_may_append_their_records_but_never_core_kinds() {
        let v = json!({"events": [
            {"ns": "Task", "name": "Seeded", "body": {"id": 1}},
            {"kind": {"type": "note", "text": "hi"}},
            {"kind": {"type": "policy_epoch", "epoch": 9, "allowed": ["host_write"], "preset": "full", "approval_ref": null}},
            {"kind": {"type": "approval_decided", "req": 1, "outcome": "allowed_once", "scope": "session"}}
        ]});
        let got = tool_events(&v);
        assert_eq!(got.len(), 2);
        assert!(matches!(&got[0], EntryKind::Custom { ns, .. } if ns == "Task"));
        assert!(matches!(&got[1], EntryKind::Note { .. }));
    }
}
