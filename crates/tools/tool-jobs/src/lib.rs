//! Every running tool call is an operation; these tools are views on them.
//! `job_wait` is how the model blocks on purpose when it has nothing else to do; it returns on
//! an `AHO_PROGRESS` / `AHO_CHECKPOINT` marker, on the terminal status, or on its timeout.
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use aho_cap::*;
use aho_log::{OpKind, OpStatus};
use aho_proc::{OpTable, OpWait, OperationsHandle};
use aho_types::*;
use serde_json::json;

pub const DEFAULT_WAIT_MS: u64 = 30_000;
pub const MAX_WAIT_MS: u64 = 600_000;

pub struct JobsDeps {
    pub ops: OperationsHandle,
}

impl Deps for JobsDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Operations];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(JobsDeps { ops: r.handle()? })
    }
}

#[derive(Clone)]
pub struct ToolJobs {
    ops: OpTable,
}

impl Capability for ToolJobs {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = JobsDeps;
    type Config = ();

    fn build(deps: JobsDeps, _: ()) -> Result<Self, BuildError> {
        Ok(ToolJobs { ops: deps.ops.0 })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        for decl in TOOLS {
            let me = self.clone();
            let name = decl.name;
            let exec: ToolExec = Arc::new(move |req: ExecRequest| {
                let me = me.clone();
                Box::pin(async move { me.run(name, &req.view.call, &req.view.args, &req.cancel).await })
            });
            r.tool(decl, exec);
        }
    }
}

declare_capability!(
    ToolJobs,
    "tool-jobs",
    kind = Kind::Tool,
    provides = [],
    requires = [],
    tools = TOOLS,
    doc = "List, read, wait on, and kill the operations behind running tool calls."
);

pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new("job_list", "List running and finished operations (tool calls) with their status and age.", Consequence::ReadOnly)
        .safe()
        .schema(schema_list)
        .op(OpKind::Compute, true, false),
    ToolDecl::new("job_output", "Read an operation's output from `cursor` (default 0). Returns the next cursor.", Consequence::ReadOnly)
        .safe()
        .schema(schema_output)
        .op(OpKind::Io, true, false),
    ToolDecl::new(
        "job_wait",
        "Block until an operation prints a progress or checkpoint marker, finishes, or `timeout_ms` passes. Use it only when you have nothing independent to do.",
        Consequence::ReadOnly,
    )
    .safe()
    .cancel_on_interrupt()
    .schema(schema_wait)
    .op(OpKind::Timer, true, false),
    ToolDecl::new("job_kill", "Cancel an operation; a process is killed with its whole process group.", Consequence::SpawnsProcess)
        .destructive()
        .schema(schema_kill)
        .op(OpKind::Compute, false, false),
];

fn object(props: Value, required: &[&str]) -> RootSchema {
    RootSchema(json!({"type": "object", "properties": props, "required": required, "additionalProperties": false}))
}
fn op_prop() -> Value {
    json!({"type": "integer", "minimum": 1, "description": "Operation id, as shown by job_list."})
}
fn schema_list(_: &BoundWorld) -> RootSchema {
    object(json!({}), &[])
}
fn schema_output(_: &BoundWorld) -> RootSchema {
    object(
        json!({"op": op_prop(), "cursor": {"type": "integer", "minimum": 0, "description": "Byte offset returned by the previous call."}}),
        &["op"],
    )
}
fn schema_wait(_: &BoundWorld) -> RootSchema {
    object(
        json!({"op": op_prop(), "timeout_ms": {"type": "integer", "minimum": 0, "maximum": MAX_WAIT_MS, "default": DEFAULT_WAIT_MS}}),
        &["op"],
    )
}
fn schema_kill(_: &BoundWorld) -> RootSchema {
    object(json!({"op": op_prop()}), &["op"])
}

fn bad_args(msg: impl Into<String>) -> ExecOutcome {
    ExecOutcome::err("TOOL_BAD_ARGS", msg)
}

fn done(value: Value, text: String) -> ExecOutcome {
    ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta: None }
}

/// Operator/model sentence for a terminal status.
pub fn render_status(s: &OpStatus) -> String {
    match s {
        OpStatus::Completed { code } => format!("completed (exit {code})"),
        OpStatus::Failed { err } => format!("failed: {err}"),
        OpStatus::Cancelled { reason } => {
            format!("cancelled ({})", serde_json::to_value(reason).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default())
        }
        OpStatus::Uncertain { .. } => "uncertain: it may or may not have happened; check its effect before repeating it".into(),
    }
}

fn status_json(s: &Option<OpStatus>) -> Value {
    s.as_ref().and_then(|s| serde_json::to_value(s).ok()).unwrap_or(Value::Null)
}

impl ToolJobs {
    fn op_arg(&self, args: &Value, own: &CallId) -> Result<OpId, ExecOutcome> {
        let op = args
            .get("op")
            .and_then(Value::as_u64)
            .map(OpId)
            .ok_or_else(|| bad_args("`op` is required and must be a positive integer; nothing was done."))?;
        if self.ops.status(op).is_none() {
            return Err(ExecOutcome::err(
                "JOB_UNKNOWN",
                format!("there is no operation {}; nothing was done. job_list shows the ids.", op.0),
            ));
        }
        if self.ops.op_for_call(own) == Some(op) {
            return Err(bad_args(format!("operation {} is this call itself; nothing was done.", op.0)));
        }
        Ok(op)
    }

    /// Execute one of `TOOLS` by name for tool call `call`.
    pub async fn run(&self, tool: &str, call: &CallId, args: &Value, cancel: &CancelToken) -> ExecOutcome {
        let r = match tool {
            "job_list" => Ok(self.list(call)),
            "job_output" => self.output(call, args),
            "job_wait" => self.wait(call, args, cancel).await,
            "job_kill" => self.kill(call, args),
            other => Err(bad_args(format!("tool-jobs has no tool `{other}`."))),
        };
        r.unwrap_or_else(|e| e)
    }

    fn list(&self, own: &CallId) -> ExecOutcome {
        let me = self.ops.op_for_call(own);
        let ages: std::collections::BTreeMap<OpId, u64> = self.ops.running().into_iter().map(|(op, _, age)| (op, age)).collect();
        let infos: Vec<_> = self.ops.list().into_iter().filter(|i| Some(i.op) != me).collect();
        let mut lines = Vec::new();
        let mut jobs = Vec::new();
        for i in &infos {
            let state = match &i.status {
                None => format!("running {:.1}s", ages.get(&i.op).copied().unwrap_or(0) as f64 / 1000.0),
                Some(s) => render_status(s),
            };
            let pid = i.identity.map(|p| format!("  pid {}", p.pid)).unwrap_or_default();
            lines.push(format!("op {}  {}  {state}{pid}  (call {})", i.op.0, i.tool, i.call.0));
            jobs.push(json!({
                "op": i.op.0, "call": i.call.0, "tool": i.tool, "kind": i.kind, "status": status_json(&i.status),
                "started": i.started.0, "age_ms": ages.get(&i.op), "pid": i.identity.map(|p| p.pid),
            }));
        }
        let text = if lines.is_empty() { "No operations.".to_string() } else { lines.join("\n") };
        done(json!({"jobs": jobs}), text)
    }

    fn output(&self, own: &CallId, args: &Value) -> Result<ExecOutcome, ExecOutcome> {
        let op = self.op_arg(args, own)?;
        let cursor = match args.get("cursor") {
            None | Some(Value::Null) => 0,
            Some(v) => v.as_u64().ok_or_else(|| bad_args("`cursor` must be a non-negative integer; nothing was read."))?,
        };
        let o = self.ops.read(op, cursor).ok_or_else(|| ExecOutcome::err("JOB_UNKNOWN", format!("operation {} disappeared.", op.0)))?;
        let mut text = String::new();
        if o.dropped > 0 {
            text.push_str(&format!("[… {} earlier bytes were dropped from the buffer …]\n", o.dropped));
        }
        text.push_str(if o.text.is_empty() { "(no new output)\n" } else { &o.text });
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("[next cursor {} · {}]", o.cursor, if o.done { "finished" } else { "still running" }));
        Ok(done(json!({"op": op.0, "text": o.text, "cursor": o.cursor, "done": o.done, "dropped": o.dropped}), text))
    }

    async fn wait(&self, own: &CallId, args: &Value, cancel: &CancelToken) -> Result<ExecOutcome, ExecOutcome> {
        let op = self.op_arg(args, own)?;
        let timeout = match args.get("timeout_ms") {
            None | Some(Value::Null) => DEFAULT_WAIT_MS,
            Some(v) => {
                v.as_u64().ok_or_else(|| bad_args("`timeout_ms` must be a non-negative integer; nothing was done."))?.min(MAX_WAIT_MS)
            }
        };
        let wait = self.ops.wait(op, timeout);
        let cancelled = async {
            while !cancel.is_cancelled() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        let ev = tokio::select! {
            ev = wait => ev,
            _ = cancelled => return Err(ExecOutcome::Cancelled),
        };
        let n = op.0;
        Ok(match ev {
            Some(OpWait::Progress(p)) => done(json!({"op": n, "event": "progress", "payload": p}), format!("op {n} progress: {p}")),
            Some(OpWait::Checkpoint(c)) => {
                let t = if c.is_empty() { format!("op {n} reached a checkpoint") } else { format!("op {n} reached a checkpoint: {c}") };
                done(json!({"op": n, "event": "checkpoint", "payload": c}), t)
            }
            Some(OpWait::Done(s)) => done(
                json!({"op": n, "event": "done", "status": status_json(&Some(s.clone()))}),
                format!("op {n} {}. Its result arrives as the call's own result; job_output reads the full output.", render_status(&s)),
            ),
            None => done(
                json!({"op": n, "event": "timeout", "timeout_ms": timeout}),
                format!(
                    "op {n} is still running after {:.1}s; nothing else happened. Wait again, or continue with other work.",
                    timeout as f64 / 1000.0
                ),
            ),
        })
    }

    fn kill(&self, own: &CallId, args: &Value) -> Result<ExecOutcome, ExecOutcome> {
        let op = self.op_arg(args, own)?;
        let info = self.ops.info(op);
        if let Some(Some(s)) = self.ops.status(op) {
            return Err(ExecOutcome::err("JOB_FINISHED", format!("operation {} already {}; nothing was killed.", op.0, render_status(&s))));
        }
        let tool = info.map(|i| i.tool).unwrap_or_default();
        if !self.ops.cancel(op) {
            return Err(ExecOutcome::err(
                "JOB_FINISHED",
                format!("operation {} finished before it could be cancelled; nothing was killed.", op.0),
            ));
        }
        Ok(done(
            json!({"op": op.0, "cancelled": true}),
            format!(
                "Cancelled op {} ({tool}); a process is killed with its whole process group. Its own result will say it was cancelled.",
                op.0
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aho_proc::{SpawnSpec, Subprocess};

    fn setup() -> (OpTable, ToolJobs) {
        let ops = OpTable::new();
        let t = ToolJobs::build(JobsDeps { ops: OperationsHandle(ops.clone()) }, ()).unwrap();
        (ops, t)
    }

    fn text(o: &ExecOutcome) -> String {
        match o {
            ExecOutcome::Done { content, .. } => content.iter().filter_map(|b| b.as_text()).collect(),
            ExecOutcome::Err { code, reason } => format!("{code}: {reason}"),
            ExecOutcome::Cancelled => "cancelled".into(),
        }
    }

    fn own(ops: &OpTable, id: &str, tool: &str) -> CallId {
        let c = CallId(id.into());
        ops.register(c.clone(), tool, OpKind::Timer, true, false);
        c
    }

    #[tokio::test]
    async fn job_wait_returns_on_progress_checkpoint_then_done() {
        let (ops, t) = setup();
        let (op, _) = ops.register(CallId("bash1".into()), "bash", OpKind::Process, false, true);
        let me = own(&ops, "w1", "job_wait");
        let feeder = ops.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            feeder.progress(op, "compiling".into());
            feeder.progress(op, "AHO_PROGRESS {\"done\":3,\"of\":9}".into());
        });
        let r = t.run("job_wait", &me, &json!({"op": op.0}), &CancelToken::new()).await;
        assert_eq!(text(&r), "op 1 progress: {\"done\":3,\"of\":9}");

        let feeder = ops.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            feeder.progress(op, "AHO_CHECKPOINT tests built".into());
        });
        let r = t.run("job_wait", &me, &json!({"op": op.0}), &CancelToken::new()).await;
        assert_eq!(text(&r), "op 1 reached a checkpoint: tests built");

        ops.finish(op, OpStatus::Completed { code: 0 });
        let r = t.run("job_wait", &me, &json!({"op": op.0}), &CancelToken::new()).await;
        assert!(text(&r).starts_with("op 1 completed (exit 0)."), "{}", text(&r));
    }

    #[tokio::test]
    async fn job_wait_times_out_honours_cancel_and_refuses_bad_targets() {
        let (ops, t) = setup();
        let (op, _) = ops.register(CallId("bash1".into()), "bash", OpKind::Process, false, true);
        let me = own(&ops, "w1", "job_wait");
        let r = t.run("job_wait", &me, &json!({"op": op.0, "timeout_ms": 40}), &CancelToken::new()).await;
        assert!(text(&r).contains("still running after 0.0s"), "{}", text(&r));

        let cancel = CancelToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            c2.cancel();
        });
        let r = t.run("job_wait", &me, &json!({"op": op.0, "timeout_ms": 60_000}), &cancel).await;
        assert_eq!(r, ExecOutcome::Cancelled);

        let mine = ops.op_for_call(&me).unwrap();
        assert!(text(&t.run("job_wait", &me, &json!({"op": mine.0}), &CancelToken::new()).await).starts_with("TOOL_BAD_ARGS"));
        assert!(text(&t.run("job_wait", &me, &json!({"op": 99}), &CancelToken::new()).await).starts_with("JOB_UNKNOWN"));
        assert!(text(&t.run("job_wait", &me, &json!({}), &CancelToken::new()).await).starts_with("TOOL_BAD_ARGS"));
    }

    #[tokio::test]
    async fn job_output_pages_with_a_cursor() {
        let (ops, t) = setup();
        let (op, _) = ops.register(CallId("bash1".into()), "bash", OpKind::Process, false, true);
        let me = own(&ops, "o1", "job_output");
        ops.progress(op, "one".into());
        let r = t.run("job_output", &me, &json!({"op": op.0}), &CancelToken::new()).await;
        assert_eq!(text(&r), "one\n[next cursor 4 · still running]");
        ops.progress(op, "two".into());
        ops.finish(op, OpStatus::Completed { code: 0 });
        let r = t.run("job_output", &me, &json!({"op": op.0, "cursor": 4}), &CancelToken::new()).await;
        assert_eq!(text(&r), "two\n[next cursor 8 · finished]");
        let r = t.run("job_output", &me, &json!({"op": op.0, "cursor": 8}), &CancelToken::new()).await;
        assert_eq!(text(&r), "(no new output)\n[next cursor 8 · finished]");
    }

    #[tokio::test]
    async fn job_list_shows_other_ops_with_status_but_not_itself() {
        let (ops, t) = setup();
        let (a, _) = ops.register(CallId("bash1".into()), "bash", OpKind::Process, false, true);
        let (b, _) = ops.register(CallId("read1".into()), "read", OpKind::Io, true, false);
        ops.finish(b, OpStatus::Failed { err: "boom".into() });
        let me = own(&ops, "l1", "job_list");
        let r = t.run("job_list", &me, &json!({}), &CancelToken::new()).await;
        let s = text(&r);
        assert!(s.contains(&format!("op {}  bash  running", a.0)), "{s}");
        assert!(s.contains(&format!("op {}  read  failed: boom", b.0)), "{s}");
        assert!(!s.contains("job_list"), "{s}");
        let ExecOutcome::Done { value, .. } = r else { panic!() };
        assert_eq!(value["jobs"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn job_kill_cancels_the_op_and_kills_its_process_group() {
        let dir = std::env::temp_dir();
        let proc = world_host::HostSubprocess::new(&dir);
        let (ops, t) = setup();
        let (op, token) = ops.register(CallId("bash1".into()), "bash", OpKind::Process, false, true);
        let child = proc.spawn(SpawnSpec::new(&["/bin/sh", "-c", "sleep 30 & sleep 30"])).await.unwrap();
        ops.attach_child(op, child.clone());
        let me = own(&ops, "k1", "job_kill");
        let r = t.run("job_kill", &me, &json!({"op": op.0}), &CancelToken::new()).await;
        assert!(text(&r).starts_with("Cancelled op 1 (bash)"), "{}", text(&r));
        assert!(token.is_cancelled());
        let exit = tokio::time::timeout(Duration::from_secs(5), child.wait()).await.unwrap().unwrap();
        assert!(exit.signal.is_some(), "{exit:?}");
        ops.finish(op, OpStatus::Cancelled { reason: aho_log::CancelReason::Aborted });
        let again = t.run("job_kill", &me, &json!({"op": op.0}), &CancelToken::new()).await;
        assert!(text(&again).starts_with("JOB_FINISHED: operation 1 already cancelled (aborted)"), "{}", text(&again));
    }

    #[test]
    fn schemas_name_their_required_fields() {
        let w = BoundWorld::host();
        for d in TOOLS {
            let s = (d.schema)(&w).0;
            let props = s["properties"].as_object().unwrap();
            for r in s["required"].as_array().unwrap() {
                assert!(props.contains_key(r.as_str().unwrap()), "{}", d.name);
            }
        }
    }
}
