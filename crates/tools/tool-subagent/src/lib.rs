//! `04`/`10`/`12`/`16` — model-facing subagent tools over the `subagent` seam.
//!
//! * `subagent` / `subagent_fork` start a child and run it to idle *inside this tool call's
//!   operation* (kind `Remote`, 16): the parent sees the fixed placeholder and keeps working;
//!   the child's last message arrives as this call's (possibly late) result.
//! * The child's policy is `aho_policy::child_policy(parent, decl_cut, is_async, bubble)` (04):
//!   `preset` narrows, nothing widens; an `async` child cannot ask the operator unless `bubble`.
//! * Lifecycle is recorded in the *parent* log (10, 12): every result that closes a child run
//!   carries `value.events = [{"kind": Subagent/Started}, {"kind": Subagent/Ended}]`, which the
//!   session appends right after the `Tool/Result`. A run is always closed, even when it is
//!   interrupted or fails, so no child row stays open forever (12).
//!
//! The classes are `ReadOnly`: a call grants nothing the child's own decision chain does not
//! re-check — every child call is reviewed under the narrowed child policy.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use aho_cap::*;
use aho_log::{EntryKind, OpKind};
use aho_policy::SandboxPolicy;
use aho_subagent::{ChildInfo, Mode, Result_, SpawnRequest, SubagentError, SubagentProvider};
use aho_types::*;
use serde_json::json;

pub struct SubagentDeps {
    pub provider: Arc<dyn SubagentProvider>,
}

impl Deps for SubagentDeps {
    const REQUIRES: &'static [HandleKind] = &[];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        r.service::<Arc<dyn SubagentProvider>>(aho_subagent::SERVICE)
            .map(|provider| SubagentDeps { provider })
            .ok_or(MissingDep(HandleKind::Custom("subagent")))
    }
}

#[derive(Clone)]
pub struct ToolSubagent {
    provider: Arc<dyn SubagentProvider>,
}

impl Capability for ToolSubagent {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = SubagentDeps;
    type Config = ();

    fn build(deps: SubagentDeps, _: ()) -> Result<Self, BuildError> {
        Ok(ToolSubagent { provider: deps.provider })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        for decl in TOOLS {
            let me = self.clone();
            let name = decl.name;
            let exec: ToolExec = Arc::new(move |req: ExecRequest| {
                let me = me.clone();
                Box::pin(async move { me.run(name, req).await })
            });
            r.tool(decl, exec);
        }
    }
}

declare_capability!(
    ToolSubagent,
    "tool-subagent",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::Seam(aho_subagent::SEAM)],
    tools = TOOLS,
    entry_kinds = ["Subagent/Started", "Subagent/Ended"],
    doc = "Start, fork, message, interrupt, and list subagents; each child runs under parent ⊓ declaration and its lifecycle is recorded in the parent log."
);

pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new(
        "subagent",
        "Start a subagent: a fresh agent with its own conversation that works on `prompt` and returns its final message. It runs in the background like any tool call, so keep working; its result arrives when it finishes. It gets this session's policy narrowed by `preset` and only the `tools` you list (all when omitted). `async: true` marks a background agent that cannot ask the operator anything unless `bubble: true`.",
        Consequence::ReadOnly,
    )
    .safe()
    .schema(schema_subagent)
    .op(OpKind::Remote, false, true),
    ToolDecl::new(
        "subagent_fork",
        "Fork this conversation into a subagent that sees everything up to the end of the previous turn (never the current one) and works on `prompt`; returns its final message. Runs in the background like `subagent`.",
        Consequence::ReadOnly,
    )
    .safe()
    .schema(schema_fork)
    .op(OpKind::Remote, false, true),
    ToolDecl::new(
        "send_message",
        "Send a follow-up to one of your subagents (by id or label). It continues its own conversation and this call returns its new final message; if it is still working, the text joins its current run.",
        Consequence::ReadOnly,
    )
    .safe()
    .schema(schema_send)
    .op(OpKind::Remote, false, true),
    ToolDecl::new(
        "interrupt_agent",
        "Interrupt a running subagent (by id or label): its running tool calls are cancelled and it is told to stop; the call that started it returns with what it had.",
        Consequence::ReadOnly,
    )
    .safe()
    .schema(schema_child)
    .op(OpKind::Compute, false, false),
    ToolDecl::new("list_agents", "List this session's subagents: id, label, depth, and whether each is running.", Consequence::ReadOnly)
        .safe()
        .schema(schema_list)
        .op(OpKind::Compute, true, false),
];

fn object(props: Value, required: &[&str]) -> RootSchema {
    RootSchema(json!({"type": "object", "properties": props, "required": required, "additionalProperties": false}))
}
fn child_prop() -> Value {
    json!({"type": ["string", "integer"], "description": "The subagent's id or label, as list_agents shows."})
}
fn schema_subagent(_: &BoundWorld) -> RootSchema {
    object(
        json!({
            "prompt": {"type": "string", "description": "The task, with every piece of context the subagent needs: it does not see this conversation."},
            "label": {"type": "string", "description": "Short name you will use to refer to it (send_message, interrupt_agent)."},
            "persona": {"type": "string", "description": "Standing instructions for the subagent (role, style, constraints)."},
            "tools": {"type": "array", "items": {"type": "string"}, "description": "Tool names it may use; omitted = all of yours."},
            "preset": {"type": "string", "enum": ["read-only", "workspace", "dev", "lab", "full", "spec"], "description": "Narrows its policy; it can never exceed yours."},
            "async": {"type": "boolean", "default": false, "description": "Background agent: it cannot ask the operator for approval unless `bubble` is true."},
            "bubble": {"type": "boolean", "default": false, "description": "Route a background agent's approval prompts to the operator."}
        }),
        &["prompt", "label"],
    )
}
fn schema_fork(_: &BoundWorld) -> RootSchema {
    object(
        json!({
            "prompt": {"type": "string", "description": "What the fork should do; it already knows the conversation up to the previous turn."},
            "label": {"type": "string", "description": "Short name for it; default `fork`."}
        }),
        &["prompt"],
    )
}
fn schema_send(_: &BoundWorld) -> RootSchema {
    object(json!({"child": child_prop(), "text": {"type": "string", "description": "The follow-up message."}}), &["child", "text"])
}
fn schema_child(_: &BoundWorld) -> RootSchema {
    object(json!({"child": child_prop()}), &["child"])
}
fn schema_list(_: &BoundWorld) -> RootSchema {
    object(json!({}), &[])
}

// ───────────────────────────── helpers ─────────────────────────────

fn bad_args(msg: impl Into<String>) -> ExecOutcome {
    ExecOutcome::err("TOOL_BAD_ARGS", msg)
}

fn done(value: Value, text: String) -> ExecOutcome {
    ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta: None }
}

fn req_str(args: &Value, key: &str, nothing: &str) -> Result<String, ExecOutcome> {
    match args.get(key).and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => Ok(s.to_string()),
        _ => Err(bad_args(format!("`{key}` is required and must be a non-empty string; {nothing}."))),
    }
}

fn opt_str(args: &Value, key: &str) -> Result<Option<String>, ExecOutcome> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(bad_args(format!("`{key}` must be a string; nothing was started."))),
    }
}

fn opt_bool(args: &Value, key: &str) -> Result<bool, ExecOutcome> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(bad_args(format!("`{key}` must be true or false; nothing was started."))),
    }
}

fn opt_str_array(args: &Value, key: &str) -> Result<Vec<String>, ExecOutcome> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| bad_args(format!("`{key}` must be a list of tool names; nothing was started.")))
            })
            .collect(),
        Some(_) => Err(bad_args(format!("`{key}` must be a list of tool names; nothing was started."))),
    }
}

fn stop_name(s: StopReason) -> String {
    serde_json::to_value(s).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| format!("{s:?}"))
}

fn sentence(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn spawn_error(e: SubagentError) -> ExecOutcome {
    match e {
        SubagentError::DepthLimit(max) => ExecOutcome::err(
            "DEPTH_LIMIT",
            format!(
                "A subagent here would be deeper than subagent.max_depth = {max}; nothing was started. Do this work yourself, or ask the operator to raise `subagent.max_depth`."
            ),
        ),
        SubagentError::Unsupported(what) => ExecOutcome::err(
            "SUBAGENT_UNSUPPORTED",
            format!("The subagent provider does not support {what}; nothing was started. Drop that option and retry."),
        ),
        SubagentError::Provider(msg) => ExecOutcome::err("SUBAGENT_FAILED", sentence(&msg)),
    }
}

fn error_text(e: &SubagentError) -> String {
    match e {
        SubagentError::DepthLimit(max) => format!("depth limit {max} reached"),
        SubagentError::Unsupported(what) => format!("unsupported: {what}"),
        SubagentError::Provider(msg) => msg.clone(),
    }
}

/// `Subagent/Started` + `Subagent/Ended` for the parent log, in the `value.events` shape the
/// session accepts (`{"kind": <EntryKind JSON>}`).
pub fn lifecycle_events(child: SessionId, provider: &str, depth: u32, stop: StopReason, last_message: Option<String>) -> Vec<Value> {
    [EntryKind::SubagentStarted { child, provider: provider.to_string(), depth }, EntryKind::SubagentEnded { child, stop, last_message }]
        .iter()
        .filter_map(|k| serde_json::to_value(k).ok())
        .map(|k| json!({ "kind": k }))
        .collect()
}

// ───────────────────────────── tools ─────────────────────────────

impl ToolSubagent {
    pub async fn run(&self, tool: &str, req: ExecRequest) -> ExecOutcome {
        let r = match tool {
            "subagent" => self.start(&req, false).await,
            "subagent_fork" => self.start(&req, true).await,
            "send_message" => self.send(&req).await,
            "interrupt_agent" => self.interrupt(&req.view.args).await,
            "list_agents" => Ok(self.list()),
            other => Err(bad_args(format!("tool-subagent has no tool `{other}`."))),
        };
        r.unwrap_or_else(|e| e)
    }

    async fn start(&self, req: &ExecRequest, fork: bool) -> Result<ExecOutcome, ExecOutcome> {
        let args = &req.view.args;
        let prompt = req_str(args, "prompt", "nothing was started")?;
        let label = match opt_str(args, "label")? {
            Some(l) => l,
            None if fork => "fork".to_string(),
            None => return Err(bad_args("`label` is required: a short name you will use to refer to the subagent; nothing was started.")),
        };
        let (persona, tools, preset, is_async, bubble) = if fork {
            (None, Vec::new(), None, false, false)
        } else {
            (
                opt_str(args, "persona")?,
                opt_str_array(args, "tools")?,
                opt_str(args, "preset")?,
                opt_bool(args, "async")?,
                opt_bool(args, "bubble")?,
            )
        };

        let features = self.provider.features();
        let unsupported = |what: &str| {
            ExecOutcome::err("SUBAGENT_UNSUPPORTED", format!("The bound subagent provider cannot {what}; nothing was started."))
        };
        if fork && !features.fork {
            return Err(unsupported("fork a conversation; use `subagent` and put the context in its prompt"));
        }
        if persona.is_some() && !features.persona {
            return Err(unsupported("take a persona; drop `persona` and put the instructions in the prompt"));
        }
        if !tools.is_empty() && !features.tool_filter {
            return Err(unsupported("restrict tools; drop `tools` or use `preset` to narrow the policy"));
        }

        let view = &req.view.policy;
        let decl_cut = match &preset {
            Some(p) => aho_policy::preset(p)
                .ok_or_else(|| {
                    bad_args(format!("`{p}` is not a preset; use read-only, workspace, dev, lab, full, or spec. Nothing was started."))
                })?
                .cut(&view.world),
            None => ConsequenceSet::ALL,
        };
        let parent = SandboxPolicy::new(view.allowed, view.world.clone(), view.workspace.clone(), view.home.clone(), view.epoch);
        let policy = aho_policy::child_policy(&parent, decl_cut, is_async, bubble);
        let fork_at = if fork {
            Some(self.provider.fork_point().ok_or_else(|| {
                ExecOutcome::err(
                    "SUBAGENT_NO_FORK_POINT",
                    "This session has no completed turn to fork from yet (a fork never inherits the turn in progress); nothing was started. Use `subagent` and put the context in its prompt.",
                )
            })?)
        } else {
            None
        };
        let spawn = SpawnRequest {
            label: label.clone(),
            prompt,
            mode: Mode::Continuable,
            persona,
            tool_filter: tools,
            model: None,
            decl_cut,
            is_async,
            bubble,
            fork_at,
            depth: 0,
        };
        let child = self.provider.spawn(spawn, policy).await.map_err(spawn_error)?;
        let result = self.wait_or_cancel(child, &req.cancel).await;
        Ok(self.finish(child, &label, result))
    }

    /// Waits for the child's run; when this call is cancelled (parent abort), interrupts the
    /// child and still waits for it to wind down so the run is closed in the parent log.
    async fn wait_or_cancel(&self, child: SessionId, cancel: &CancelToken) -> Result<Result_, SubagentError> {
        let wait = self.provider.wait(child);
        tokio::pin!(wait);
        let mut interrupted = false;
        loop {
            tokio::select! {
                r = &mut wait => return r,
                _ = tokio::time::sleep(Duration::from_millis(50)), if !interrupted => {
                    if cancel.is_cancelled() {
                        interrupted = true;
                        let _ = self.provider.interrupt(child).await;
                    }
                }
            }
        }
    }

    fn finish(&self, child: SessionId, label: &str, result: Result<Result_, SubagentError>) -> ExecOutcome {
        let info = self.provider.child_info(child);
        let (provider, depth) = info.map(|i| (i.provider, i.depth)).unwrap_or_else(|| ("unknown".to_string(), 0));
        let (stop, last, diag) = match result {
            Ok(r) => (r.stop, r.last_message, r.diagnostic),
            Err(e) => (StopReason::Error, None, Some(error_text(&e))),
        };
        let events = lifecycle_events(child, &provider, depth, stop, last.clone());
        let mut text = format!("[subagent `{label}` (id {}) finished: {}]\n", child.0, stop_name(stop));
        text.push_str(last.as_deref().unwrap_or("(the subagent ended without a final message)"));
        if let Some(d) = &diag {
            text.push_str(&format!("\n[diagnostic] {d}"));
        }
        done(
            json!({
                "child": child.0.to_string(),
                "label": label,
                "provider": provider,
                "depth": depth,
                "stop": stop_name(stop),
                "last_message": last,
                "diagnostic": diag,
                "events": events,
            }),
            text,
        )
    }

    fn resolve_child(&self, args: &Value, nothing: &str) -> Result<ChildInfo, ExecOutcome> {
        let key = match args.get("child") {
            Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
            Some(Value::Number(n)) => n.to_string(),
            _ => return Err(bad_args(format!("`child` is required: the subagent's id or label; {nothing}."))),
        };
        let all = self.provider.children();
        let by_id = key.parse::<u128>().ok().and_then(|id| all.iter().find(|c| c.child.0 == id));
        // A label names the most recent child that carries it.
        let found = by_id.or_else(|| all.iter().rev().find(|c| c.label == key));
        found.cloned().ok_or_else(|| {
            ExecOutcome::err(
                "SUBAGENT_UNKNOWN",
                format!("There is no subagent `{key}` in this session; {nothing}. list_agents shows ids and labels."),
            )
        })
    }

    async fn send(&self, req: &ExecRequest) -> Result<ExecOutcome, ExecOutcome> {
        let args = &req.view.args;
        let info = self.resolve_child(args, "nothing was sent")?;
        let text = req_str(args, "text", "nothing was sent")?;
        self.provider.send(info.child, text).await.map_err(|e| ExecOutcome::err("SUBAGENT_FAILED", sentence(&error_text(&e))))?;
        let result = self.wait_or_cancel(info.child, &req.cancel).await;
        Ok(self.finish(info.child, &info.label, result))
    }

    async fn interrupt(&self, args: &Value) -> Result<ExecOutcome, ExecOutcome> {
        let info = self.resolve_child(args, "nothing was interrupted")?;
        if !info.running {
            return Ok(done(
                json!({"child": info.child.0.to_string(), "interrupted": false}),
                format!("Subagent `{}` (id {}) is idle; there was nothing to interrupt.", info.label, info.child.0),
            ));
        }
        self.provider.interrupt(info.child).await.map_err(|e| ExecOutcome::err("SUBAGENT_FAILED", sentence(&error_text(&e))))?;
        Ok(done(
            json!({"child": info.child.0.to_string(), "interrupted": true}),
            format!(
                "Interrupted subagent `{}` (id {}). The call that started it returns with what it had once it winds down.",
                info.label, info.child.0
            ),
        ))
    }

    fn list(&self) -> ExecOutcome {
        let all = self.provider.children();
        if all.is_empty() {
            return done(json!({"agents": []}), "No subagents in this session.".into());
        }
        let lines: Vec<String> = all
            .iter()
            .map(|c| {
                let state = if c.running {
                    "running".to_string()
                } else {
                    c.last_stop.map(|s| format!("idle (last run: {})", stop_name(s))).unwrap_or_else(|| "idle".into())
                };
                format!("id {}  `{}`  depth {}  {state}  ({})", c.child.0, c.label, c.depth, c.provider)
            })
            .collect();
        let agents: Vec<Value> = all
            .iter()
            .map(|c| {
                json!({
                    "child": c.child.0.to_string(), "label": c.label, "depth": c.depth, "running": c.running,
                    "last_stop": c.last_stop.map(stop_name), "provider": c.provider,
                })
            })
            .collect();
        done(json!({ "agents": agents }), lines.join("\n"))
    }
}
