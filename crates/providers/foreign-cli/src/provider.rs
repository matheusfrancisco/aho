//! The engine behind every foreign-CLI `SubagentProvider`: children, runs, and the seam calls.
//! The per-child loop that tails and watches lives in `drive.rs`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use aho_cap::{HookEvent, HookOutput, HookSpec, OnError};
use aho_log::LogStore;
use aho_policy::SandboxPolicy;
use aho_subagent::{ChildInfo, Features, Mode, Result_, SpawnRequest, SubagentError, SubagentProvider};
use aho_types::{BoxFuture, Consequence, Hash, SessionId, StopReason, Timestamp};
use serde_json::json;
use tokio::sync::watch;

use crate::event::{Record, clip};
use crate::mirror::{self, DIAGNOSTIC_MAX, Mirror};
use crate::recover::{self, Recovered};
use crate::settings::{Settings, which};
use crate::tail::Tail;
use crate::tmux::Tmux;

/// One foreign CLI: how to launch it, find its transcript, and read that transcript.
pub trait Cli: Send + Sync + 'static {
    /// Capability id; also `Subagent/Started.provider`.
    const PROVIDER: &'static str;
    /// Product name for operator messages (`Claude Code`).
    const PRODUCT: &'static str;
    /// Short name used in tmux session names (`claude` → `aho-claude-<child>`).
    const SHORT: &'static str;
    /// Whether it can take standing instructions (`SpawnRequest::persona`).
    const PERSONA: bool;

    /// The command line and environment for one child.
    fn launch(&self, ctx: &LaunchCtx<'_>) -> Result<Launch, String>;
    /// This child's transcript, once the CLI has written it.
    fn locate(&self, ctx: &LocateCtx<'_>) -> Option<PathBuf>;
    /// One transcript line → events, or an explicit reason it is ignored.
    fn normalize(&self, line: &[u8]) -> Record;
}

pub struct LaunchCtx<'a> {
    pub child: SessionId,
    /// Canonical working directory (the child's workspace).
    pub cwd: &'a Path,
    pub prompt: &'a str,
    pub persona: Option<&'a str>,
    pub model: Option<&'a str>,
    /// The child's policy allows `WorkspaceWrite`; otherwise the CLI is started read-only.
    pub writable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    /// `argv[0]` is the resolved binary.
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    /// The CLI's session id, when aho chooses it up front.
    pub cli_session: Option<String>,
}

pub struct LocateCtx<'a> {
    pub cwd: &'a Path,
    pub cli_session: Option<&'a str>,
    /// Launch time (ms since the epoch); older transcripts are not it.
    pub launched_ms: u64,
    /// Transcripts other children already own.
    pub claimed: &'a [PathBuf],
}

/// A child's lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Started in tmux; its transcript has not appeared yet.
    Launching,
    /// Its transcript is being tailed.
    Live,
    /// After a crash it was launching: whether it runs is unknown (blocked + uncertain).
    Uncertain,
    /// Exited, closed, or gone.
    Closed,
}

#[derive(Clone, Default)]
pub(crate) struct Run {
    /// Runs started; 0 = never.
    pub runs: u64,
    /// `None` while the current run is going.
    pub result: Option<Result_>,
}

pub(crate) struct Track {
    pub status: Status,
    pub tail: Option<Tail>,
    pub cli_session: Option<String>,
    pub version: Option<String>,
    pub last_text: Option<String>,
    pub errors: Vec<String>,
    pub unconfirmed: VecDeque<u64>,
    pub next_seq: u64,
    pub created: Option<u64>,
    /// Last transcript growth, send, or run start.
    pub quiet_since: Instant,
    pub screen: Option<String>,
    pub screen_since: Instant,
    pub blocked: bool,
    pub missing_scans: u32,
    pub interrupt_at: Option<Instant>,
    pub last_check: Option<Instant>,
    pub io_error: Option<String>,
    pub closed_reason: Option<String>,
}

impl Track {
    fn new(status: Status) -> Track {
        let now = Instant::now();
        Track {
            status,
            tail: None,
            cli_session: None,
            version: None,
            last_text: None,
            errors: Vec::new(),
            unconfirmed: VecDeque::new(),
            next_seq: 1,
            created: None,
            quiet_since: now,
            screen: None,
            screen_since: now,
            blocked: false,
            missing_scans: 0,
            interrupt_at: None,
            last_check: None,
            io_error: None,
            closed_reason: None,
        }
    }
}

pub(crate) struct Child {
    pub id: SessionId,
    pub label: String,
    pub depth: u32,
    pub mode: Mode,
    pub session: String,
    pub cwd: PathBuf,
    pub launched_ms: u64,
    pub state: watch::Sender<Run>,
    pub track: Mutex<Track>,
    pub driving: AtomicBool,
}

impl Child {
    pub fn running(&self) -> bool {
        let r = self.state.borrow();
        r.runs > 0 && r.result.is_none()
    }
    fn info(&self, provider: &str) -> ChildInfo {
        let r = self.state.borrow();
        ChildInfo {
            child: self.id,
            provider: provider.into(),
            label: self.label.clone(),
            depth: self.depth,
            running: r.runs > 0 && r.result.is_none(),
            last_stop: r.result.as_ref().map(|x| x.stop),
        }
    }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

pub(crate) struct Engine<C: Cli> {
    pub cli: C,
    pub settings: Settings,
    pub tmux: Tmux,
    pub mirror: Mirror,
    pub children: Mutex<Vec<Arc<Child>>>,
    seq: AtomicU64,
}

/// A foreign CLI as a `SubagentProvider` (register it as `aho_subagent::SERVICE`).
pub struct ForeignProvider<C: Cli> {
    pub(crate) eng: Arc<Engine<C>>,
}

impl<C: Cli> Clone for ForeignProvider<C> {
    fn clone(&self) -> Self {
        ForeignProvider { eng: self.eng.clone() }
    }
}

fn provider_err(e: impl std::fmt::Display) -> SubagentError {
    SubagentError::Provider(e.to_string())
}

impl<C: Cli> Engine<C> {
    pub fn record(&self, name: &str, child: SessionId, body: serde_json::Value, t: &mut Track) {
        if let Err(e) = self.mirror.append(name, child, body) {
            t.errors.push(e);
        }
    }

    /// Ends the current run with `result` (no-op when no run is going) and records it.
    pub fn finish_run(&self, c: &Child, t: &mut Track, stop: StopReason, diagnostic: Option<String>) {
        if !c.running() {
            return;
        }
        let mut diag: Vec<String> = std::mem::take(&mut t.errors);
        diag.extend(diagnostic);
        let diagnostic = (!diag.is_empty()).then(|| clip(&diag.join("\n"), DIAGNOSTIC_MAX - 4).0);
        let result = Result_ { child: c.id, stop, last_message: t.last_text.clone(), diagnostic };
        let run = c.state.borrow().runs;
        let body = json!({
            "run": run,
            "stop": stop,
            "last_message": result.last_message.as_deref().map(|m| clip(m, mirror::TEXT_MAX).0),
            "diagnostic": result.diagnostic,
        });
        self.record(mirror::RESULT, c.id, body, t);
        t.interrupt_at = None;
        c.state.send_modify(|r| r.result = Some(result));
    }

    pub fn start_run(&self, c: &Child, t: &mut Track) {
        c.state.send_modify(|r| {
            r.runs += 1;
            r.result = None;
        });
        t.last_text = None;
        t.errors.clear();
        t.blocked = false;
        t.screen = None;
        t.quiet_since = Instant::now();
    }

    /// Marks the child closed, ending a run that is still going.
    pub fn close_child(&self, c: &Child, t: &mut Track, stop: StopReason, reason: &str, exit_status: Option<i32>) {
        if t.status == Status::Closed {
            return;
        }
        self.finish_run(c, t, stop, Some(reason.to_string()));
        t.status = Status::Closed;
        t.closed_reason = Some(reason.to_string());
        self.record(mirror::CLOSED, c.id, json!({"reason": reason, "exit_status": exit_status}), t);
    }

    fn find(&self, child: SessionId) -> Option<Arc<Child>> {
        lock(&self.children).iter().find(|c| c.id == child).cloned()
    }

    fn get(&self, child: SessionId) -> Result<Arc<Child>, SubagentError> {
        self.find(child).ok_or_else(|| {
            provider_err(format!("there is no {} subagent {} in this session; list_agents shows the live ones.", C::PRODUCT, child.0))
        })
    }

    pub fn claimed(&self, except: SessionId) -> Vec<PathBuf> {
        lock(&self.children)
            .iter()
            .filter(|c| c.id != except)
            .filter_map(|c| lock(&c.track).tail.as_ref().map(|t| t.path().to_path_buf()))
            .collect()
    }

    fn next_id(&self) -> SessionId {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let h = Hash::of_json(&(C::PROVIDER, n, Timestamp::now().0, std::process::id()));
        let mut b = [0u8; 8];
        b.copy_from_slice(&h.0[..8]);
        let v = u64::from_le_bytes(b) & ((1u64 << 53) - 1);
        SessionId(u128::from(v.max(1)))
    }
}

/// Refuses policies a host CLI would escape; returns whether the child may write its workspace.
fn check_policy(product: &str, policy: &SandboxPolicy) -> Result<bool, SubagentError> {
    let world = policy.world();
    if world.confines {
        return Err(provider_err(format!(
            "{product} runs on the host with your own credentials, and the bound world `{}` confines tools, so it would escape that world; nothing was started. Bind `seams.subagent = \"subagent-inproc\"` for children inside the world.",
            world.name
        )));
    }
    let allowed = policy.allowed();
    let missing: Vec<&str> =
        [Consequence::SpawnsProcess, Consequence::NetworkEgress].into_iter().filter(|c| !allowed.contains(*c)).map(|c| c.name()).collect();
    if !missing.is_empty() {
        return Err(provider_err(format!(
            "{product} runs its own tools and talks to its vendor's API, which needs {}; this child's policy (preset `{}`) does not allow that, so nothing was started. Ask for preset `dev` or wider, or bind `seams.subagent = \"subagent-inproc\"`.",
            missing.join(" and "),
            policy.preset().name()
        )));
    }
    Ok(allowed.contains(Consequence::WorkspaceWrite))
}

impl<C: Cli> ForeignProvider<C> {
    /// Builds the provider over the parent log and folds back every child an earlier process
    /// started (crash-honest receipts). Recovered children start being tailed on the first
    /// async call or at `SessionStart` ([`ForeignProvider::resume`]).
    pub fn new(cli: C, settings: Settings, log: Arc<dyn LogStore>) -> Result<ForeignProvider<C>, String> {
        let tmux = Tmux::new(which(&settings.tmux).unwrap_or_else(|| PathBuf::from(&settings.tmux)), settings.socket.clone());
        let eng = Arc::new(Engine {
            cli,
            settings,
            tmux,
            mirror: Mirror::new(log.clone(), C::PROVIDER),
            children: Mutex::new(Vec::new()),
            seq: AtomicU64::new(0),
        });
        let entries = match log.head() {
            Some(h) => log.read_branch(h).map_err(|e| format!("could not read the session log to recover subagents ({e})"))?,
            None => Vec::new(),
        };
        for r in recover::scan(&entries, C::PROVIDER) {
            if !r.closed {
                eng.adopt(r)?;
            }
        }
        Ok(ForeignProvider { eng })
    }

    /// Starts tailing every recovered child that is not being tailed yet. Needs a tokio runtime.
    pub async fn resume(&self) {
        let all: Vec<Arc<Child>> = lock(&self.eng.children).clone();
        for c in all {
            crate::drive::ensure(&self.eng, &c);
        }
    }

    /// Ends a child: its tmux session is killed (the CLI keeps its own transcript, so it can be
    /// resumed by hand), a going run is closed as aborted, and it leaves `children()`.
    pub async fn close(&self, child: SessionId, reason: &str) -> Result<(), SubagentError> {
        let c = self.eng.get(child)?;
        crate::drive::pump(&self.eng, &c);
        let killed = self.eng.tmux.kill(&c.session).await;
        {
            let mut t = lock(&c.track);
            if let Err(e) = &killed {
                t.errors.push(e.clone());
            }
            self.eng.close_child(&c, &mut t, StopReason::Aborted, reason, None);
        }
        lock(&self.eng.children).retain(|x| x.id != child);
        killed.map_err(provider_err)
    }

    /// Closes every child (the parent session is ending).
    pub async fn close_all(&self, reason: &str) {
        let ids: Vec<SessionId> = lock(&self.eng.children).iter().map(|c| c.id).collect();
        for id in ids {
            if let Err(e) = self.close(id, reason).await {
                let c = self.eng.find(id);
                if let Some(c) = c {
                    lock(&c.track).errors.push(e_text(&e));
                }
            }
        }
    }

    pub fn status(&self, child: SessionId) -> Option<Status> {
        self.eng.find(child).map(|c| lock(&c.track).status)
    }

    /// The transcript being tailed, once found.
    pub fn transcript(&self, child: SessionId) -> Option<PathBuf> {
        self.eng.find(child).and_then(|c| lock(&c.track).tail.as_ref().map(|t| t.path().to_path_buf()))
    }

    /// The tmux session name and the command that attaches to it.
    pub fn tmux_session(&self, child: SessionId) -> Option<(String, String)> {
        self.eng.find(child).map(|c| (c.session.clone(), self.eng.tmux.attach_hint(&c.session)))
    }

    /// `SessionStart`: resume recovered children. `SessionEnd`: close them all.
    pub fn hooks(&self) -> [HookSpec; 2] {
        let start = self.clone();
        let end = self.clone();
        [
            HookSpec {
                on: &[HookEvent::SessionStart],
                if_class: None,
                if_tool: None,
                timeout_ms: 5_000,
                once: true,
                on_error: OnError::Allow,
                run: Arc::new(move |_, _| {
                    let p = start.clone();
                    Box::pin(async move {
                        p.resume().await;
                        HookOutput::default()
                    })
                }),
            },
            HookSpec {
                on: &[HookEvent::SessionEnd],
                if_class: None,
                if_tool: None,
                timeout_ms: 30_000,
                once: true,
                on_error: OnError::Allow,
                run: Arc::new(move |_, _| {
                    let p = end.clone();
                    Box::pin(async move {
                        p.close_all("the parent session ended").await;
                        HookOutput::default()
                    })
                }),
            },
        ]
    }

    #[allow(clippy::disallowed_methods, reason = "authority: foreign-cli-workspace")]
    async fn spawn_inner(&self, req: SpawnRequest, policy: SandboxPolicy) -> Result<SessionId, SubagentError> {
        self.resume().await;
        let eng = &self.eng;
        let depth = aho_subagent::depth(1, req.depth);
        if depth > eng.settings.max_depth {
            return Err(SubagentError::DepthLimit(eng.settings.max_depth));
        }
        if req.fork_at.is_some() {
            return Err(SubagentError::Unsupported(
                "forking this conversation (a foreign CLI cannot inherit it; put the context in the prompt)",
            ));
        }
        if !req.tool_filter.is_empty() {
            return Err(SubagentError::Unsupported("tool filters (a foreign CLI uses its own tools; narrow it with a preset)"));
        }
        if req.persona.is_some() && !C::PERSONA {
            return Err(SubagentError::Unsupported("personas"));
        }
        if req.prompt.trim().is_empty() {
            return Err(provider_err("the subagent prompt is empty; nothing was started. Say what the child should do."));
        }
        let writable = check_policy(C::PRODUCT, &policy)?;
        let cwd = std::fs::canonicalize(policy.workspace())
            .map_err(|e| provider_err(format!("the child's workspace {} is not usable ({e}); nothing was started.", policy.workspace())))?;
        let id = eng.next_id();
        let launch = eng
            .cli
            .launch(&LaunchCtx {
                child: id,
                cwd: &cwd,
                prompt: &req.prompt,
                persona: req.persona.as_deref(),
                model: req.model.as_deref(),
                writable,
            })
            .map_err(provider_err)?;
        let session = format!("aho-{}-{}", C::SHORT, id.0);
        let launched_ms = Timestamp::now().0;
        let receipt = json!({
            "label": req.label, "depth": depth, "cwd": cwd.display().to_string(), "tmux_socket": eng.tmux.socket(),
            "tmux_session": session, "cli_session": launch.cli_session, "state": "launching",
        });
        eng.mirror.append(mirror::RECEIPT, id, receipt).map_err(|e| provider_err(format!("{e}; nothing was started")))?;
        if let Err(e) = eng.tmux.launch(&session, &cwd, &launch.env, &launch.argv).await {
            let reason = format!("{} could not be started in tmux: {e}", C::PRODUCT);
            let mut t = Track::new(Status::Closed);
            eng.record(mirror::CLOSED, id, json!({"reason": reason}), &mut t);
            let bin = launch.argv.first().map(String::as_str).unwrap_or("?");
            return Err(provider_err(format!("{reason}; nothing is running. Check `{}` and `{bin}`.", eng.settings.tmux)));
        }
        let mut t = Track::new(Status::Launching);
        t.cli_session = launch.cli_session.clone();
        t.unconfirmed.push_back(0);
        match eng.tmux.session(&session).await {
            Ok(Some(s)) => {
                t.created = Some(s.created);
                let body = json!({"tmux_session": session, "created": s.created, "pane_pid": s.pane_pid, "attach": eng.tmux.attach_hint(&session)});
                eng.record(mirror::LAUNCHED, id, body, &mut t);
            }
            Ok(None) => t.errors.push(format!("tmux does not list session {session} right after starting it")),
            Err(e) => t.errors.push(e),
        }
        let child = Arc::new(Child {
            id,
            label: req.label,
            depth,
            mode: req.mode,
            session,
            cwd,
            launched_ms,
            state: watch::channel(Run { runs: 1, result: None }).0,
            track: Mutex::new(t),
            driving: AtomicBool::new(false),
        });
        lock(&eng.children).push(child.clone());
        crate::drive::ensure(eng, &child);
        Ok(id)
    }

    async fn send_inner(&self, child: SessionId, text: String) -> Result<(), SubagentError> {
        self.resume().await;
        let eng = &self.eng;
        let c = eng.get(child)?;
        let (seq, starts_run) = {
            let mut t = lock(&c.track);
            match t.status {
                Status::Closed => {
                    return Err(provider_err(format!(
                        "{} subagent {} has ended ({}); nothing was sent. Start a new one.",
                        C::PRODUCT,
                        child.0,
                        t.closed_reason.clone().unwrap_or_else(|| "closed".into())
                    )));
                }
                Status::Uncertain => {
                    return Err(provider_err(format!(
                        "{} subagent {} was launching when aho stopped, so whether it is running is unknown; nothing was sent. interrupt_agent closes its tmux session `{}`; then start a new one.",
                        C::PRODUCT,
                        child.0,
                        c.session
                    )));
                }
                Status::Launching | Status::Live => {}
            }
            let starts_run = !c.running();
            if starts_run {
                eng.start_run(&c, &mut t);
            }
            let seq = t.next_seq;
            t.next_seq += 1;
            t.quiet_since = Instant::now();
            t.blocked = false;
            eng.record(mirror::SENT, c.id, json!({"seq": seq, "starts_run": starts_run, "chars": text.chars().count()}), &mut t);
            (seq, starts_run)
        };
        if let Err(e) = eng.tmux.submit(&c.session, &text).await {
            let msg = format!("could not type into {}'s pane ({e}); the text was not sent", C::PRODUCT);
            let mut t = lock(&c.track);
            if starts_run {
                eng.finish_run(&c, &mut t, StopReason::Error, Some(msg.clone()));
            }
            return Err(provider_err(msg));
        }
        lock(&c.track).unconfirmed.push_back(seq);
        Ok(())
    }

    async fn interrupt_inner(&self, child: SessionId) -> Result<(), SubagentError> {
        self.resume().await;
        let eng = &self.eng;
        let c = eng.get(child)?;
        let status = lock(&c.track).status;
        if status == Status::Uncertain {
            return self.close(child, "interrupted after a crash left it uncertain").await;
        }
        if !c.running() || status == Status::Closed {
            return Ok(());
        }
        eng.tmux.keys(&c.session, &["Escape"]).await.map_err(|e| provider_err(format!("could not interrupt {}: {e}", C::PRODUCT)))?;
        let mut t = lock(&c.track);
        t.interrupt_at = Some(Instant::now());
        eng.record(mirror::NOTE, c.id, json!({"reason": "interrupt: pressed Escape in its pane"}), &mut t);
        Ok(())
    }

    async fn wait_inner(&self, child: SessionId) -> Result<Result_, SubagentError> {
        self.resume().await;
        let c = self.eng.get(child)?;
        let mut rx = c.state.subscribe();
        let done = rx.wait_for(|r| r.runs > 0 && r.result.is_some()).await.map(|r| r.result.clone());
        let result = done.ok().flatten().ok_or_else(|| provider_err(format!("subagent {} went away before it finished", child.0)))?;
        if c.mode == Mode::OneShot {
            self.close(child, "one-shot run finished").await?;
        }
        Ok(result)
    }
}

fn e_text(e: &SubagentError) -> String {
    match e {
        SubagentError::DepthLimit(d) => format!("depth limit {d}"),
        SubagentError::Unsupported(w) => format!("unsupported: {w}"),
        SubagentError::Provider(m) => m.clone(),
    }
}

impl<C: Cli> Engine<C> {
    /// Re-creates a child from the parent log after a restart.
    fn adopt(&self, r: Recovered) -> Result<(), String> {
        let uncertain = r.transcript.is_none();
        let mut t = Track::new(if uncertain { Status::Uncertain } else { Status::Live });
        t.cli_session = r.cli_session.clone();
        t.created = r.created;
        t.next_seq = r.next_seq;
        t.last_text = r.last_text.clone();
        let result = (!r.run_open).then(|| {
            let (stop, last_message) = r.last_result.clone().unwrap_or((StopReason::EndTurn, None));
            Result_ { child: r.child, stop, last_message, diagnostic: None }
        });
        let child = Arc::new(Child {
            id: r.child,
            label: r.label.clone(),
            depth: r.depth,
            mode: Mode::Continuable,
            session: r.tmux_session.clone(),
            cwd: r.cwd.clone(),
            launched_ms: r.launched_ms,
            state: watch::channel(Run { runs: r.runs, result }).0,
            track: Mutex::new(Track::new(Status::Launching)),
            driving: AtomicBool::new(false),
        });
        for seq in &r.unconfirmed {
            self.record(mirror::DELIVERY, r.child, json!({"seq": seq, "delivery": "unknown"}), &mut t);
        }
        if uncertain {
            let reason = format!(
                "aho stopped while {} was launching in tmux session `{}`; whether it came up is unknown, so it is blocked. interrupt_agent closes that session; then start a new one.",
                C::PRODUCT,
                r.tmux_session
            );
            self.record(mirror::RECOVERED, r.child, json!({"state": "blocked", "certainty": "uncertain", "reason": reason}), &mut t);
            self.finish_run(&child, &mut t, StopReason::Error, Some(reason));
        } else {
            t.unconfirmed = r.unconfirmed.iter().copied().collect();
            let path = r.transcript.clone().unwrap_or_default();
            t.tail = Some(Tail::resume(path.clone(), r.offset, r.ident));
            let reason = format!(
                "resuming the transcript {} at byte {}; its tmux session `{}` must be missing on two scans before it counts as gone",
                path.display(),
                r.offset,
                r.tmux_session
            );
            self.record(
                mirror::RECOVERED,
                r.child,
                json!({"state": "resumed", "certainty": "pending", "reason": reason, "offset": r.offset}),
                &mut t,
            );
        }
        if let Some(e) = t.errors.first() {
            return Err(e.clone());
        }
        *lock(&child.track) = t;
        lock(&self.children).push(child);
        Ok(())
    }
}

impl<C: Cli> SubagentProvider for ForeignProvider<C> {
    fn features(&self) -> Features {
        Features { fork: false, persona: C::PERSONA, tool_filter: false, output_schema: false, max_depth: self.eng.settings.max_depth }
    }

    fn spawn(&self, req: SpawnRequest, policy: SandboxPolicy) -> BoxFuture<'_, Result<SessionId, SubagentError>> {
        Box::pin(self.spawn_inner(req, policy))
    }

    fn send(&self, child: SessionId, text: String) -> BoxFuture<'_, Result<(), SubagentError>> {
        Box::pin(self.send_inner(child, text))
    }

    fn interrupt(&self, child: SessionId) -> BoxFuture<'_, Result<(), SubagentError>> {
        Box::pin(self.interrupt_inner(child))
    }

    fn wait(&self, child: SessionId) -> BoxFuture<'_, Result<Result_, SubagentError>> {
        Box::pin(self.wait_inner(child))
    }

    fn child_info(&self, child: SessionId) -> Option<ChildInfo> {
        self.eng.find(child).map(|c| c.info(C::PROVIDER))
    }

    fn children(&self) -> Vec<ChildInfo> {
        lock(&self.eng.children).iter().map(|c| c.info(C::PROVIDER)).collect()
    }
}
