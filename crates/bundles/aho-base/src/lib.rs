//! `spine()`: the composition every product includes. A product is a Rust value.
//!
//! Model adapters (OpenAI chat-completions, Anthropic messages, the deterministic `scripted`
//! route), the host, namespace (Linux) and ssh worlds (`seams.world` picks one, host by
//! default), the userspace sandbox network (`net`, started only when a sandbox world attaches),
//! the host terminals behind `tool-terminal` (opt-in) and `run_code` (programmatic tool calls),
//! the fs / bash / jobs tools, in-process subagents and their tools (`seams.subagent =
//! subagent-inproc` by default), skills (SKILL.md directories, discovered into the tree), MCP
//! servers over stdio and HTTP with their resource tools, the task-graph, goal, todo, schedule,
//! workflow and plan-mode tools, web fetch/search, LSP, Claude Code hooks compatibility (opt-in),
//! the verify gate (`verify-gate`: before a turn that changed the workspace stops, the project's
//! check runs in the bound world; a failure keeps the turn going once, `verify.enabled` turns it
//! off), and the self-inspection tools (`aho_why`, `aho_inspect`, `unlock_request`) the session points
//! the model at when a tool is missing, plus the engineer defaults that sit at `Layer::Bundle`:
//! every operator layer (profile, files, env, flags) still shadows them.
//! Products built on the spine `include` it; `Composition::include` carries these defaults along.
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

use std::sync::Arc;

use aho_cap::Composition;
use serde_json::json;

pub const NAME: &str = "aho-base";

/// The spine's fragment of built-in profile `coding` (`aho --profile coding`): turns off the
/// non-coding tool families; the operator's `~/.aho/profiles/coding.toml` layers over it.
pub const CODING_PROFILE: &str = include_str!("../profiles/coding.toml");

/// Engineer defaults (`Layer::Bundle`). The product ships under `workspace`, with a 1 s grace
/// window for async tool calls and a 10 min heartbeat while only calls are pending.
/// `tool-terminal` stays Unlocked until `aho enable tool-terminal`; Claude Code hooks run
/// commands a repository ships, so they also wait for the operator's `aho enable hooks-claude-code`;
/// telemetry is opt-in the same way (`aho enable telemetry`) and additionally needs an endpoint.
/// The job digest (`jobs-digest`: outcomes arriving close together reach the model as one
/// notice) is opt-in the same way (`aho enable jobs-digest`).
/// Jobs: the model may send a subagent run (`subagent`, `subagent_fork`, `send_message`) or a
/// program (`run_code`) to the background, and a `bash` command still running after 60 s becomes
/// a job by itself. `run_code` is never promoted on a timer: a job's nested calls cannot ask the
/// operator, so a program detached mid-run would start failing the asks it was written to make.
pub fn defaults() -> [(&'static str, serde_json::Value); 14] {
    [
        ("policy.preset", json!("workspace")),
        ("seams.world", json!("host")),
        ("seams.subagent", json!("subagent-inproc")),
        ("tools.grace_ms", json!(1000)),
        ("tools.heartbeat_s", json!(600)),
        ("caps.tool-terminal.enabled", json!(false)),
        ("caps.hooks-claude-code.enabled", json!(false)),
        ("caps.telemetry.enabled", json!(false)),
        ("caps.jobs-digest.enabled", json!(false)),
        ("jobs.detach.subagent", json!("on_request")),
        ("jobs.detach.subagent_fork", json!("on_request")),
        ("jobs.detach.send_message", json!("on_request")),
        ("jobs.detach.bash", json!("after:60s")),
        ("jobs.detach.run_code", json!("on_request")),
    ]
}

pub fn spine() -> Composition {
    let c = Composition::new(NAME)
        .provide::<llm_openai_completions::OpenAiCompletions>()
        .provide::<llm_anthropic_messages::AnthropicMessages>()
        .provide::<llm_openai_responses::OpenAiResponses>()
        .provide::<llm_pi_messages::PiMessages>()
        .provide::<llm_scripted::Scripted>()
        .provide::<world_host::HostWorld>()
        .provide::<world_ssh::SshWorld>()
        .provide::<world_ns::NamespaceWorld>()
        .provide::<net_userspace::UserspaceNet>()
        .provide::<pty_host::PtyHost>()
        .consume::<tool_terminal::ToolTerminal>()
        .consume::<tool_run_code::ToolRunCode>()
        .consume::<tool_fs::ToolFs>()
        .consume::<context_files::ContextFiles>()
        .consume::<tool_bash::ToolBash>()
        .consume::<tool_jobs::ToolJobs>()
        .consume::<tool_lsp::ToolLsp>()
        .provide::<subagent_inproc::SubagentInproc>()
        .provide::<subagent_acp::SubagentAcp>()
        .consume::<tool_subagent::ToolSubagent>()
        .provide::<skills_fsdir::SkillsFsdir>()
        .consume::<tool_skills::ToolSkills>()
        .discoverer(Arc::new(skills_fsdir::FsSkills::new()))
        .provide::<mcp_stdio::McpStdio>()
        .provide::<mcp_http::McpHttp>()
        .consume::<tool_mcp_resources::ToolMcpResources>()
        .consume::<tool_tasks::ToolTasks>()
        .consume::<tool_goals::ToolGoals>()
        .consume::<tool_todo::ToolTodo>()
        .consume::<tool_schedule::ToolSchedule>()
        .consume::<tool_workflow::ToolWorkflow>()
        .consume::<tool_plan_mode::ToolPlanMode>()
        .consume::<tool_inspect::ToolInspect>()
        .provide::<tool_web::WebFetch>()
        .provide::<tool_web::WebSearch>()
        .provide::<hooks_claude_code::ClaudeCodeHooks>()
        .provide::<verify_gate::VerifyGate>()
        .profile("coding", CODING_PROFILE)
        .provide::<telemetry::Telemetry>()
        .provide::<jobs_digest::JobsDigest>();
    defaults().into_iter().fold(c, |c, (k, v)| c.default(k, v))
}

#[cfg(test)]
mod tests;
