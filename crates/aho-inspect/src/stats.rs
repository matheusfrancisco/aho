//! `aho stats` and the telemetry vocabulary: both are folds over the same log entries.
//!
//! Two views, one derivation:
//! * [`stats`] — exact local aggregates for the operator (`aho stats`, `sidebar.stats`).
//!   Numbers and tool names are precise; nothing here leaves the machine.
//! * [`events`] — the closed, bucketed export vocabulary (docs `12`, Engineering discipline).
//!   [`Event`] has **no `String` field anywhere**: every value is a unit variant of a closed
//!   enum, so a path, prompt or third-party tool name is unrepresentable, not filtered.
//!   `tests::telemetry_events_serialize_only_vocabulary_tokens` pins the whole surface.

use std::collections::BTreeMap;

use aho_log::{ApprovalOutcome, CapDeltaOp, Entry, EntryId, EntryKind, GrantScope, OpKind, OpStatus, ToolOutcome, Transition};
use aho_types::{CallId, Hash, OpId, StopReason, Timestamp, Usage};
use serde::Serialize;

// ───────────────────────────── local aggregates (`aho stats`) ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolStat {
    pub tool: String,
    pub calls: u64,
    pub ok: u64,
    pub errors: u64,
    pub cancelled: u64,
    /// Results that landed after the turn that issued them (`ToolResult.late`).
    pub late: u64,
    pub p50_ms: Option<u64>,
    pub max_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ApprovalStat {
    pub requested: u64,
    pub allowed: u64,
    pub rejected: u64,
    pub cancelled: u64,
    /// Median time a card waited for the operator.
    pub p50_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct OpStat {
    pub completed: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub uncertain: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Stats {
    pub turns: u64,
    /// Wall clock from the first to the last entry on the branch.
    pub duration_ms: Option<u64>,
    pub usage: Usage,
    /// `cache_read / (input + cache_read)`, when any input tokens were counted.
    pub cache_hit_pct: Option<u8>,
    pub requests: u64,
    pub request_errors: u64,
    pub model_p50_ms: Option<u64>,
    pub model_max_ms: Option<u64>,
    /// Per tool, sorted by call count (busiest first), then name.
    pub tools: Vec<ToolStat>,
    pub approvals: ApprovalStat,
    pub ops: OpStat,
    pub compactions: u64,
    pub compaction_failures: u64,
    pub subagents: u64,
    /// `StepEnd` transitions by name, busiest first.
    pub transitions: Vec<(String, u64)>,
}

fn median(sorted: &mut [u64]) -> Option<u64> {
    sorted.sort_unstable();
    if sorted.is_empty() { None } else { Some(sorted[sorted.len() / 2]) }
}

/// Exact aggregates over one branch, oldest first (as `read_branch` returns it).
pub fn stats(entries: &[Entry]) -> Stats {
    let mut s = Stats::default();
    let mut calls: BTreeMap<CallId, (String, Timestamp)> = BTreeMap::new();
    let mut approvals: BTreeMap<EntryId, Timestamp> = BTreeMap::new();
    let mut open_request: Option<Timestamp> = None;
    let mut per_tool: BTreeMap<String, (ToolStat, Vec<u64>)> = BTreeMap::new();
    let mut model_lat: Vec<u64> = Vec::new();
    let mut approval_lat: Vec<u64> = Vec::new();
    let mut transitions: BTreeMap<String, u64> = BTreeMap::new();

    for e in entries {
        match &e.kind {
            EntryKind::TurnEnd { .. } => s.turns += 1,
            EntryKind::StepEnd { transition, .. } => {
                *transitions.entry(transition_class(transition).token().to_string()).or_default() += 1;
            }
            EntryKind::RequestHeader { .. } => {
                s.requests += 1;
                open_request = Some(e.at);
            }
            EntryKind::AssistantMessage { usage, .. } => {
                s.usage = s.usage + *usage;
                if let Some(at) = open_request.take() {
                    model_lat.push(e.at.0.saturating_sub(at.0));
                }
            }
            EntryKind::AssistantAttempt { .. } => {
                s.request_errors += 1;
                open_request = None;
            }
            EntryKind::ToolCall { call, tool, .. } => {
                calls.insert(call.clone(), (tool.clone(), e.at));
            }
            EntryKind::ToolResult { call, outcome, late, .. } => {
                if let Some((tool, at)) = calls.remove(call) {
                    let (row, lat) = per_tool.entry(tool.clone()).or_insert_with(|| {
                        (ToolStat { tool, calls: 0, ok: 0, errors: 0, cancelled: 0, late: 0, p50_ms: None, max_ms: None }, Vec::new())
                    });
                    row.calls += 1;
                    match outcome {
                        ToolOutcome::Ok => row.ok += 1,
                        ToolOutcome::Err { .. } => row.errors += 1,
                        ToolOutcome::Cancelled { .. } => row.cancelled += 1,
                    }
                    if *late {
                        row.late += 1;
                    }
                    lat.push(e.at.0.saturating_sub(at.0));
                }
            }
            EntryKind::ApprovalRequested { .. } => {
                s.approvals.requested += 1;
                approvals.insert(e.id, e.at);
            }
            EntryKind::ApprovalDecided { req, outcome, .. } => {
                match outcome {
                    ApprovalOutcome::AllowedOnce => s.approvals.allowed += 1,
                    ApprovalOutcome::Rejected => s.approvals.rejected += 1,
                    ApprovalOutcome::Cancelled | ApprovalOutcome::Unavailable => s.approvals.cancelled += 1,
                }
                if let Some(at) = approvals.remove(req) {
                    approval_lat.push(e.at.0.saturating_sub(at.0));
                }
            }
            EntryKind::OpDone { status, .. } => match status {
                OpStatus::Completed { .. } => s.ops.completed += 1,
                OpStatus::Failed { .. } => s.ops.failed += 1,
                OpStatus::Cancelled { .. } => s.ops.cancelled += 1,
                OpStatus::Uncertain { .. } => s.ops.uncertain += 1,
            },
            EntryKind::CompactionEnd { error, .. } => {
                s.compactions += 1;
                if error.is_some() {
                    s.compaction_failures += 1;
                }
            }
            EntryKind::SubagentStarted { .. } => s.subagents += 1,
            _ => {}
        }
    }

    s.duration_ms = match (entries.first(), entries.last()) {
        (Some(a), Some(b)) => Some(b.at.0.saturating_sub(a.at.0)),
        _ => None,
    };
    let denom = s.usage.input_tokens + s.usage.cache_read_tokens;
    s.cache_hit_pct = (s.usage.cache_read_tokens * 100).checked_div(denom).map(|p| p as u8);
    s.model_max_ms = model_lat.iter().max().copied();
    s.model_p50_ms = median(&mut model_lat);
    s.approvals.p50_ms = median(&mut approval_lat);
    s.tools = per_tool
        .into_values()
        .map(|(mut row, mut lat)| {
            row.max_ms = lat.iter().max().copied();
            row.p50_ms = median(&mut lat);
            row
        })
        .collect();
    s.tools.sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.tool.cmp(&b.tool)));
    let mut tr: Vec<(String, u64)> = transitions.into_iter().collect();
    tr.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    s.transitions = tr;
    s
}

fn human_ms(ms: u64) -> String {
    match ms {
        0..=999 => format!("{ms}ms"),
        1_000..=59_999 => format!("{}.{}s", ms / 1000, ms % 1000 / 100),
        60_000..=3_599_999 => format!("{}m{:02}s", ms / 60_000, ms % 60_000 / 1000),
        _ => format!("{}h{:02}m", ms / 3_600_000, ms % 3_600_000 / 60_000),
    }
}

pub fn render_stats(s: &Stats) -> String {
    let dur = s.duration_ms.map(human_ms).unwrap_or_else(|| "-".into());
    let mut out = format!("session {dur}   turns: {}   subagents: {}\n", s.turns, s.subagents);
    let cache = s.cache_hit_pct.map(|p| format!("   cache {p}%")).unwrap_or_default();
    let p50 = s.model_p50_ms.map(human_ms).unwrap_or_else(|| "-".into());
    out.push_str(&format!(
        "model    {} request(s) · p50 {p50} · {} error(s){cache}   tokens: in {} / out {}\n",
        s.requests, s.request_errors, s.usage.input_tokens, s.usage.output_tokens
    ));
    if !s.tools.is_empty() {
        out.push_str("tools (busiest first):\n");
        for t in &s.tools {
            let p50 = t.p50_ms.map(human_ms).unwrap_or_else(|| "-".into());
            let max = t.max_ms.map(human_ms).unwrap_or_else(|| "-".into());
            let late = if t.late > 0 { format!(" · late {}", t.late) } else { String::new() };
            out.push_str(&format!(
                "  {:<24}{:>4} call(s)   ok {} · err {} · cancelled {}{late}   p50 {p50} · max {max}\n",
                t.tool, t.calls, t.ok, t.errors, t.cancelled
            ));
        }
    }
    if s.approvals.requested > 0 {
        let p50 = s.approvals.p50_ms.map(human_ms).unwrap_or_else(|| "-".into());
        out.push_str(&format!(
            "approvals {} requested · {} allowed · {} rejected · {} cancelled · waited p50 {p50}\n",
            s.approvals.requested, s.approvals.allowed, s.approvals.rejected, s.approvals.cancelled
        ));
    }
    let o = &s.ops;
    if o.completed + o.failed + o.cancelled + o.uncertain > 0 {
        out.push_str(&format!(
            "ops      {} completed · {} failed · {} cancelled · {} uncertain\n",
            o.completed, o.failed, o.cancelled, o.uncertain
        ));
    }
    if s.compactions > 0 {
        out.push_str(&format!("compactions {} ({} failed)\n", s.compactions, s.compaction_failures));
    }
    if !s.transitions.is_empty() {
        let list: Vec<String> = s.transitions.iter().map(|(k, n)| format!("{k} {n}")).collect();
        out.push_str(&format!("steps    {}\n", list.join(" · ")));
    }
    out
}

// ───────────────────────────── telemetry vocabulary ─────────────────────────────
//
// Every field of `Event` is a closed unit-variant enum (or `bool`); adding a `String` field
// is the reviewable act of breaking the privacy contract. Buckets follow docs `12`:
// coarse enough to be useless for identification, fine enough to answer "is it slow".

macro_rules! tokens {
    ($name:ident { $($var:ident => $tok:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
        pub enum $name {
            $(#[serde(rename = $tok)] $var,)+
        }
        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$var),+];
            pub fn token(self) -> &'static str {
                match self { $($name::$var => $tok,)+ }
            }
        }
    };
}

tokens!(LatencyBucket {
    Lt100ms => "<100ms",
    Lt1s => "<1s",
    Lt10s => "<10s",
    Lt1m => "<1m",
    Ge1m => "1m+",
});

tokens!(DurationBucket {
    Lt1m => "<1m",
    Lt10m => "<10m",
    Lt1h => "<1h",
    Ge1h => "1h+",
});

tokens!(CountBucket {
    C0 => "0",
    C1 => "1",
    C2to5 => "2-5",
    C6to20 => "6-20",
    C21to100 => "21-100",
    Gt100 => "100+",
});

tokens!(PctBucket {
    Zero => "0",
    Lt50 => "<50",
    Lt90 => "<90",
    Ge90 => "90+",
});

tokens!(ToolClass {
    Fs => "fs",
    Bash => "bash",
    Terminal => "terminal",
    Jobs => "jobs",
    Web => "web",
    Vm => "vm",
    Display => "display",
    Formal => "formal",
    Goals => "goals",
    Inspect => "inspect",
    Lsp => "lsp",
    Mcp => "mcp",
    Memory => "memory",
    PlanMode => "plan_mode",
    RunCode => "run_code",
    Schedule => "schedule",
    SessionQuery => "session_query",
    Skills => "skills",
    Subagent => "subagent",
    Tasks => "tasks",
    Todo => "todo",
    Workflow => "workflow",
    Other => "other",
});

tokens!(ToolOutcomeClass {
    Ok => "ok",
    Err => "err",
    Cancelled => "cancelled",
});

tokens!(TransitionClass {
    Completed => "completed",
    ToolUse => "tool_use",
    WaitingOnOps => "waiting_on_ops",
    Retry => "retry",
    StopHook => "stop_hook",
    QueuedCommand => "queued_command",
    BudgetContinue => "budget_continue",
    PromptTooLong => "prompt_too_long",
    MaxTurns => "max_turns",
    Reload => "reload",
    Aborted => "aborted",
});

tokens!(OpOutcome {
    Completed => "completed",
    Failed => "failed",
    Cancelled => "cancelled",
    Uncertain => "uncertain",
});

tokens!(CapChange {
    Activated => "activated",
    Deactivated => "deactivated",
    Broken => "broken",
    Replaced => "replaced",
    Rejected => "rejected",
});

tokens!(StopClass {
    EndTurn => "end_turn",
    ToolUse => "tool_use",
    MaxTokens => "max_tokens",
    Refusal => "refusal",
    Aborted => "aborted",
    Error => "error",
});

tokens!(ApprovalClass {
    AllowedOnce => "allowed_once",
    Rejected => "rejected",
    Cancelled => "cancelled",
    Unavailable => "unavailable",
});

tokens!(ScopeClass {
    Once => "once",
    Session => "session",
    UntilRevoked => "until_revoked",
});

tokens!(OpClass {
    Process => "process",
    Io => "io",
    Timer => "timer",
    Remote => "remote",
    Compute => "compute",
});

/// One exportable fact. Tag and every value serialize to tokens in [`VOCAB`]; nothing else
/// can appear on the wire because no variant carries a `String`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(tag = "e", rename_all = "snake_case")]
pub enum Event {
    ModelResponse { stop: StopClass, latency: LatencyBucket, cache: PctBucket },
    ToolFinished { tool: ToolClass, outcome: ToolOutcomeClass, latency: LatencyBucket, late: bool },
    ApprovalResolved { outcome: ApprovalClass, scope: ScopeClass, latency: LatencyBucket },
    OpClosed { kind: OpClass, status: OpOutcome, latency: LatencyBucket },
    CapChanged { op: CapChange },
    TurnEnded { tools: CountBucket, duration: DurationBucket, transition: TransitionClass },
    SessionSnapshot { turns: CountBucket, duration: DurationBucket, cache: PctBucket },
}

pub fn latency_bucket(ms: u64) -> LatencyBucket {
    match ms {
        0..=99 => LatencyBucket::Lt100ms,
        100..=999 => LatencyBucket::Lt1s,
        1_000..=9_999 => LatencyBucket::Lt10s,
        10_000..=59_999 => LatencyBucket::Lt1m,
        _ => LatencyBucket::Ge1m,
    }
}

pub fn duration_bucket(ms: u64) -> DurationBucket {
    match ms {
        0..=59_999 => DurationBucket::Lt1m,
        60_000..=599_999 => DurationBucket::Lt10m,
        600_000..=3_599_999 => DurationBucket::Lt1h,
        _ => DurationBucket::Ge1h,
    }
}

pub fn count_bucket(n: u64) -> CountBucket {
    match n {
        0 => CountBucket::C0,
        1 => CountBucket::C1,
        2..=5 => CountBucket::C2to5,
        6..=20 => CountBucket::C6to20,
        21..=100 => CountBucket::C21to100,
        _ => CountBucket::Gt100,
    }
}

pub fn pct_bucket(pct: u8) -> PctBucket {
    match pct {
        0 => PctBucket::Zero,
        1..=49 => PctBucket::Lt50,
        50..=89 => PctBucket::Lt90,
        _ => PctBucket::Ge90,
    }
}

/// First-party names map to their class; `mcp__*` collapses to `mcp`; anything unknown —
/// including every third-party tool name — collapses to `other` and never leaves the machine.
pub fn tool_class(name: &str) -> ToolClass {
    if name.starts_with("mcp__") {
        return ToolClass::Mcp;
    }
    if name.starts_with("vm_") {
        return ToolClass::Vm;
    }
    match name {
        "read" | "write" | "edit" | "glob" | "grep" | "read_image" | "str_replace_editor" => ToolClass::Fs,
        "bash" | "bash_persistent" => ToolClass::Bash,
        n if n.starts_with("terminal_") => ToolClass::Terminal,
        n if n.starts_with("job_") => ToolClass::Jobs,
        "web_fetch" | "web_search" => ToolClass::Web,
        "screen_capture" | "pointer" | "mouse" | "keyboard" => ToolClass::Display,
        "spec_check" | "spec_trace" | "lean_build" | "lean_translate" | "invariant_add" | "invariant_mutate" => ToolClass::Formal,
        "create_goal" | "get_goal" | "update_goal" => ToolClass::Goals,
        "aho_why" | "aho_inspect" | "unlock_request" => ToolClass::Inspect,
        n if n.starts_with("lsp_") => ToolClass::Lsp,
        "list_mcp_resources" | "list_mcp_resource_templates" | "read_mcp_resource" => ToolClass::Mcp,
        "memory_search" | "memory_save" => ToolClass::Memory,
        "enter_plan_mode" | "exit_plan_mode" => ToolClass::PlanMode,
        "run_code" => ToolClass::RunCode,
        n if n.starts_with("schedule_") => ToolClass::Schedule,
        "session_search" | "session_trace" | "event_get" | "event_list" => ToolClass::SessionQuery,
        "skill" => ToolClass::Skills,
        "subagent" | "subagent_fork" | "send_message" | "interrupt_agent" | "list_agents" => ToolClass::Subagent,
        n if n.starts_with("task_") => ToolClass::Tasks,
        "todo_write" | "todo_note" => ToolClass::Todo,
        n if n.starts_with("ralph_") => ToolClass::Workflow,
        _ => ToolClass::Other,
    }
}

fn transition_class(t: &Transition) -> TransitionClass {
    match t {
        Transition::Completed => TransitionClass::Completed,
        Transition::ToolUse => TransitionClass::ToolUse,
        Transition::WaitingOnOps => TransitionClass::WaitingOnOps,
        Transition::ReactiveCompactRetry | Transition::EmptyStopRetry | Transition::MaxOutputRecovery => TransitionClass::Retry,
        Transition::StopHookBlocking => TransitionClass::StopHook,
        Transition::QueuedCommand => TransitionClass::QueuedCommand,
        Transition::BudgetContinue => TransitionClass::BudgetContinue,
        Transition::PromptTooLong => TransitionClass::PromptTooLong,
        Transition::MaxTurns => TransitionClass::MaxTurns,
        Transition::Reload => TransitionClass::Reload,
        Transition::Aborted(_) => TransitionClass::Aborted,
    }
}

fn stop_class(s: StopReason) -> StopClass {
    match s {
        StopReason::EndTurn => StopClass::EndTurn,
        StopReason::ToolUse => StopClass::ToolUse,
        StopReason::MaxTokens => StopClass::MaxTokens,
        StopReason::Refusal => StopClass::Refusal,
        StopReason::Aborted => StopClass::Aborted,
        StopReason::Error => StopClass::Error,
    }
}

fn cache_pct(u: Usage) -> PctBucket {
    match (u.cache_read_tokens * 100).checked_div(u.input_tokens + u.cache_read_tokens) {
        None => PctBucket::Zero,
        Some(p) => pct_bucket(p as u8),
    }
}

/// Derives the export events for one branch, in log order. Pure; callers that flush
/// incrementally slice `entries` after the last exported `EntryId` themselves.
pub fn events(entries: &[Entry]) -> Vec<Event> {
    let mut out = Vec::new();
    let mut calls: BTreeMap<CallId, (String, Timestamp)> = BTreeMap::new();
    let mut approvals: BTreeMap<EntryId, Timestamp> = BTreeMap::new();
    let mut ops: BTreeMap<OpId, (OpKind, Timestamp)> = BTreeMap::new();
    let mut open_request: Option<Timestamp> = None;
    let mut turn_started: Option<Timestamp> = None;
    let mut turn_tools: u64 = 0;
    let mut turn_transition = TransitionClass::Completed;

    for e in entries {
        match &e.kind {
            EntryKind::TurnStart { .. } => {
                turn_started = Some(e.at);
                turn_tools = 0;
                turn_transition = TransitionClass::Completed;
            }
            EntryKind::StepEnd { transition, .. } => turn_transition = transition_class(transition),
            EntryKind::TurnEnd { .. } => {
                if let Some(at) = turn_started.take() {
                    out.push(Event::TurnEnded {
                        tools: count_bucket(turn_tools),
                        duration: duration_bucket(e.at.0.saturating_sub(at.0)),
                        transition: turn_transition,
                    });
                }
            }
            EntryKind::RequestHeader { .. } => open_request = Some(e.at),
            EntryKind::AssistantMessage { usage, stop, .. } => {
                if let Some(at) = open_request.take() {
                    out.push(Event::ModelResponse {
                        stop: stop_class(*stop),
                        latency: latency_bucket(e.at.0.saturating_sub(at.0)),
                        cache: cache_pct(*usage),
                    });
                }
            }
            EntryKind::AssistantAttempt { .. } => {
                if let Some(at) = open_request.take() {
                    out.push(Event::ModelResponse {
                        stop: StopClass::Error,
                        latency: latency_bucket(e.at.0.saturating_sub(at.0)),
                        cache: PctBucket::Zero,
                    });
                }
            }
            EntryKind::ToolCall { call, tool, .. } => {
                turn_tools += 1;
                calls.insert(call.clone(), (tool.clone(), e.at));
            }
            EntryKind::ToolResult { call, outcome, late, .. } => {
                if let Some((tool, at)) = calls.remove(call) {
                    out.push(Event::ToolFinished {
                        tool: tool_class(&tool),
                        outcome: match outcome {
                            ToolOutcome::Ok => ToolOutcomeClass::Ok,
                            ToolOutcome::Err { .. } => ToolOutcomeClass::Err,
                            ToolOutcome::Cancelled { .. } => ToolOutcomeClass::Cancelled,
                        },
                        latency: latency_bucket(e.at.0.saturating_sub(at.0)),
                        late: *late,
                    });
                }
            }
            EntryKind::ApprovalRequested { .. } => {
                approvals.insert(e.id, e.at);
            }
            EntryKind::ApprovalDecided { req, outcome, scope } => {
                let latency = approvals.remove(req).map(|at| latency_bucket(e.at.0.saturating_sub(at.0)));
                out.push(Event::ApprovalResolved {
                    outcome: match outcome {
                        ApprovalOutcome::AllowedOnce => ApprovalClass::AllowedOnce,
                        ApprovalOutcome::Rejected => ApprovalClass::Rejected,
                        ApprovalOutcome::Cancelled => ApprovalClass::Cancelled,
                        ApprovalOutcome::Unavailable => ApprovalClass::Unavailable,
                    },
                    scope: match scope {
                        GrantScope::Once => ScopeClass::Once,
                        GrantScope::Session => ScopeClass::Session,
                        GrantScope::UntilRevoked => ScopeClass::UntilRevoked,
                    },
                    latency: latency.unwrap_or(LatencyBucket::Lt100ms),
                });
            }
            EntryKind::OpStarted { op, kind, .. } => {
                ops.insert(*op, (*kind, e.at));
            }
            EntryKind::OpDone { op, status } => {
                if let Some((kind, at)) = ops.remove(op) {
                    out.push(Event::OpClosed {
                        kind: match kind {
                            OpKind::Process => OpClass::Process,
                            OpKind::Io => OpClass::Io,
                            OpKind::Timer => OpClass::Timer,
                            OpKind::Remote => OpClass::Remote,
                            OpKind::Compute => OpClass::Compute,
                        },
                        status: match status {
                            OpStatus::Completed { .. } => OpOutcome::Completed,
                            OpStatus::Failed { .. } => OpOutcome::Failed,
                            OpStatus::Cancelled { .. } => OpOutcome::Cancelled,
                            OpStatus::Uncertain { .. } => OpOutcome::Uncertain,
                        },
                        latency: latency_bucket(e.at.0.saturating_sub(at.0)),
                    });
                }
            }
            EntryKind::CapDelta { op, .. } => {
                out.push(Event::CapChanged {
                    op: match op {
                        CapDeltaOp::Activated => CapChange::Activated,
                        CapDeltaOp::Deactivated => CapChange::Deactivated,
                        CapDeltaOp::Broken { .. } => CapChange::Broken,
                        CapDeltaOp::Replaced { .. } => CapChange::Replaced,
                        CapDeltaOp::Rejected { .. } => CapChange::Rejected,
                    },
                });
            }
            _ => {}
        }
    }
    out
}

/// The whole-branch summary event a `SessionEnd` flush appends after the incremental ones.
pub fn session_snapshot(entries: &[Entry]) -> Event {
    let s = stats(entries);
    Event::SessionSnapshot {
        turns: count_bucket(s.turns),
        duration: duration_bucket(s.duration_ms.unwrap_or(0)),
        cache: cache_pct(s.usage),
    }
}

// ───────────────────────────── envelope ─────────────────────────────

/// The pure envelope fields; `is_ci` / `ran_from_cargo` are probed by the caller that holds
/// the authority to read the environment (`telemetry` provider, `aho telemetry preview`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Envelope {
    /// `CARGO_PKG_VERSION` of the workspace.
    pub version: &'static str,
    /// [`vocab_hash`]; the receiver rejects events whose vocabulary it does not know.
    pub schema: Hash,
    /// `"<weekday>/<hour>"` UTC — the only time resolution that ever leaves the machine.
    pub hw: String,
    pub is_ci: bool,
    pub ran_from_cargo: bool,
}

/// `"tue/14"`: weekday and hour (UTC) of a millisecond timestamp. 1970-01-01 was a Thursday.
pub fn hour_weekday(t: Timestamp) -> String {
    const DAYS: [&str; 7] = ["thu", "fri", "sat", "sun", "mon", "tue", "wed"];
    let days = (t.0 / 86_400_000) as usize;
    let hour = t.0 / 3_600_000 % 24;
    format!("{}/{hour:02}", DAYS[days % 7])
}

pub fn envelope(now: Timestamp, is_ci: bool, ran_from_cargo: bool) -> Envelope {
    Envelope { version: env!("CARGO_PKG_VERSION"), schema: vocab_hash(), hw: hour_weekday(now), is_ci, ran_from_cargo }
}

/// Every string that may appear in a serialized [`Event`]: tags, field names, and the closed
/// value tokens. The leak test walks real payloads against this list; the hash goes into the
/// envelope so vocabulary drift is a schema change, never a silent widening.
pub fn vocab() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = vec![
        // tags and field names
        "e",
        "model_response",
        "tool_finished",
        "approval_resolved",
        "op_closed",
        "cap_changed",
        "turn_ended",
        "session_snapshot",
        "stop",
        "latency",
        "cache",
        "tool",
        "outcome",
        "late",
        "scope",
        "kind",
        "status",
        "op",
        "tools",
        "duration",
        "transition",
        "turns",
    ];
    v.extend(LatencyBucket::ALL.iter().map(|x| x.token()));
    v.extend(DurationBucket::ALL.iter().map(|x| x.token()));
    v.extend(CountBucket::ALL.iter().map(|x| x.token()));
    v.extend(PctBucket::ALL.iter().map(|x| x.token()));
    v.extend(ToolClass::ALL.iter().map(|x| x.token()));
    v.extend(ToolOutcomeClass::ALL.iter().map(|x| x.token()));
    v.extend(TransitionClass::ALL.iter().map(|x| x.token()));
    v.extend(OpOutcome::ALL.iter().map(|x| x.token()));
    v.extend(CapChange::ALL.iter().map(|x| x.token()));
    v.extend(StopClass::ALL.iter().map(|x| x.token()));
    v.extend(ApprovalClass::ALL.iter().map(|x| x.token()));
    v.extend(ScopeClass::ALL.iter().map(|x| x.token()));
    v.extend(OpClass::ALL.iter().map(|x| x.token()));
    v.sort_unstable();
    v.dedup();
    v
}

pub fn vocab_hash() -> Hash {
    Hash::of_json(&vocab())
}
