//! TLC as the TLA+ backend of the Formal seam: `java -cp tla2tools.jar tlc2.TLC`,
//! with artifacts and processes in the same bound world through `FsWrite` and `Subprocess`.
//!
//! * `check` writes a `.cfg` from the `CheckConfig` (and, for `Bounded(n)`, a wrapper module
//!   that bounds behaviors to `n` steps) into a scratch dir next to the spec, runs TLC, and
//!   parses its console output into a closed verdict with the counterexample trace.
//! * `trace_validate` writes the trace as a TLA+ tuple in a module that steps through it with
//!   the spec's `TraceStep(e)`; a deadlock means the log is not a behavior of the spec.
//! * `mutate` checks textual mutants of the spec's actions; a mutant is caught when TLC
//!   reports a violation.

#![forbid(unsafe_code)]

pub mod model;
pub mod mutate;
pub mod output;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_formal::{
    CheckConfig, CheckMode, Counterexample, Formal, FormalError, FormalHandle, FormalKind, MutantOutcome, MutationReport, Outcome, SpecRef,
    Translation, Verdict,
};
use aho_fs::{FsWrite, ScratchDir};
use aho_proc::{ProcError, SpawnSpec, SubprocessHandle};
use aho_types::{BoxFuture, Hash, Value};
use serde_json::json;

use crate::output::{TlcResult, TraceState, Violation};

pub const INSTALL: &str = "aho doctor formal --install tla";
const OUTPUT_TAIL: usize = 4000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlcConfig {
    pub jar: PathBuf,
    /// Java launcher (a path, or a name on the world's PATH).
    pub java: String,
    pub workers: u32,
    /// Budget for runs without a `CheckConfig` of their own (trace validation).
    pub budget_ms: u64,
    pub max_mutants: usize,
}

impl Config for TlcConfig {
    const PREFIX: &'static str = "formal.tla";
    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                "formal.tla.jar",
                Ty::String,
                None,
                "Path to tla2tools.jar. `aho doctor formal --install tla` downloads it to ~/.aho/tools/tla2tools.jar and sets this key.",
            ),
            ConfigKey::new(
                "formal.tla.java",
                Ty::String,
                Some(json!("java")),
                "Java launcher that runs TLC: a path, or a name on the world's PATH.",
            ),
            ConfigKey::new("formal.tla.workers", Ty::Int, Some(json!(1)), "TLC worker threads (`-workers`)."),
            ConfigKey::new(
                "formal.tla.budget_ms",
                Ty::Int,
                Some(json!(120_000)),
                "Wall-clock budget for one TLC run when the caller gives none (trace validation).",
            ),
            ConfigKey::new("formal.tla.max_mutants", Ty::Int, Some(json!(24)), "Most mutants one `invariant_mutate` run checks."),
        ]
    }
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let jar = l.str("formal.tla.jar").filter(|s| !s.is_empty()).ok_or_else(|| ConfigError::MissingKey("formal.tla.jar".into()))?;
        let int = |k: &str, d: u64| match l.get(k) {
            None => Ok(d),
            Some(v) => v.as_u64().ok_or(ConfigError::Type { key: k.into(), expected: "a non-negative integer" }),
        };
        Ok(TlcConfig {
            jar: PathBuf::from(jar),
            java: cfg_str(l, "formal.tla.java", "java"),
            workers: int("formal.tla.workers", 1)?.clamp(1, 256) as u32,
            budget_ms: int("formal.tla.budget_ms", 120_000)?,
            max_mutants: int("formal.tla.max_mutants", 24)? as usize,
        })
    }
}

/// The world's filesystem and process spawner: artifacts and TLC share one world.
pub struct TlcDeps {
    pub proc: SubprocessHandle,
    pub fs: FsWrite,
}

impl Deps for TlcDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Subprocess, HandleKind::FsWrite];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(TlcDeps { proc: r.handle::<SubprocessHandle>()?, fs: r.handle::<FsWrite>()? })
    }
}

#[derive(Clone)]
pub struct FormalTlc {
    inner: Arc<Tlc>,
}

impl Capability for FormalTlc {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = TlcDeps;
    type Config = TlcConfig;
    fn build(deps: TlcDeps, cfg: TlcConfig) -> Result<Self, BuildError> {
        Ok(FormalTlc { inner: Arc::new(Tlc::new(deps.proc, deps.fs, cfg)) })
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.service(aho_formal::service_name(FormalKind::Tla), FormalHandle(self.inner.clone()));
    }
}

declare_capability!(
    FormalTlc,
    "formal-tlc",
    kind = Kind::Provider,
    provides = [aho_formal::TLA_SEAM],
    requires = [Requirement::Binary("java"), Requirement::Setting { key: "formal.tla.jar", pred: Pred::Set }],
    tools = &[],
    unlock = Fix::cli(INSTALL),
    enforced_by = "the bound world (artifact IO uses FsWrite; TLC runs through Subprocess)",
    doc = "TLC model checker for TLA+ specs: bounded checks, trace validation of session logs, mutation testing."
);

/// The TLC backend. Public so tests and `aho doctor` can drive it without a composition.
pub struct Tlc {
    proc: SubprocessHandle,
    fs: FsWrite,
    cfg: TlcConfig,
}

/// A provider-owned scratch lease next to the spec in the bound world.
struct WorkDir {
    lease: Box<dyn ScratchDir>,
    fs: FsWrite,
}

impl WorkDir {
    async fn next_to(fs: &FsWrite, spec: &SpecRef) -> Result<WorkDir, FormalError> {
        let lease = fs.0.scratch_dir(&spec.dir().display().to_string(), ".aho-tlc-").await.map_err(|e| {
            FormalError::Tool(format!("could not create a scratch directory next to {}: {e}; nothing was checked", spec.path.display()))
        })?;
        Ok(WorkDir { lease, fs: fs.clone() })
    }
    fn path(&self) -> &Path {
        Path::new(self.lease.path())
    }
    async fn write(&self, name: &str, text: &str) -> Result<PathBuf, FormalError> {
        let p = self.path().join(name);
        self.fs
            .0
            .write(&p.display().to_string(), text, None)
            .await
            .map_err(|e| FormalError::Tool(format!("could not write {}: {e}; nothing was checked", p.display())))?;
        Ok(p)
    }
    async fn close(&mut self) -> Result<(), FormalError> {
        self.lease.close().await.map_err(|e| FormalError::Tool(format!("could not remove TLC scratch directory: {e}")))
    }
}

async fn read_spec(fs: &FsWrite, spec: &SpecRef) -> Result<String, FormalError> {
    if spec.kind != FormalKind::Tla {
        return Err(FormalError::Unsupported(format!(
            "formal-tlc checks TLA+ modules; {} is a Lean module (use lean_build)",
            spec.path.display()
        )));
    }
    fs.0.read(&spec.path.display().to_string(), None)
        .await
        .map(|r| r.text)
        .map_err(|e| FormalError::Invalid(format!("could not read the spec {}: {e}; nothing was checked", spec.path.display())))
}

impl Tlc {
    pub fn new(proc: SubprocessHandle, fs: FsWrite, mut cfg: TlcConfig) -> Tlc {
        if cfg.jar.is_relative() {
            cfg.jar = Path::new(&fs.0.root()).join(&cfg.jar);
        }
        Tlc { proc, fs, cfg }
    }

    /// One TLC run: `main` is the module file, `cfg` the config file, `lib` the spec's dir.
    async fn run(
        &self,
        work: &WorkDir,
        main: &Path,
        cfg: &Path,
        lib: &Path,
        budget_ms: u64,
    ) -> Result<(TlcResult, String, u64), FormalError> {
        let probe = vec!["/bin/sh".into(), "-c".into(), "test -f \"$1\"".into(), "aho-tlc".into(), self.cfg.jar.display().to_string()];
        let (exists, _) = run_process(&self.proc, probe, work.path(), budget_ms, "TLC jar probe").await?;
        if exists != 0 {
            return Err(FormalError::ToolMissing {
                tool: format!("tla2tools.jar (formal.tla.jar = {})", self.cfg.jar.display()),
                fix: INSTALL.into(),
            });
        }
        let argv = vec![
            self.cfg.java.clone(),
            "-XX:+UseParallelGC".into(),
            format!("-DTLA-Library={}", lib.display()),
            "-cp".into(),
            self.cfg.jar.display().to_string(),
            "tlc2.TLC".into(),
            "-noGenerateSpecTE".into(),
            "-nowarning".into(),
            "-workers".into(),
            self.cfg.workers.to_string(),
            "-metadir".into(),
            work.path().join("states").display().to_string(),
            "-config".into(),
            cfg.display().to_string(),
            main.display().to_string(),
        ];
        let started = Instant::now();
        let (code, out) = run_process(&self.proc, argv, work.path(), budget_ms, "java (for TLC)").await?;
        let cost = started.elapsed().as_millis() as u64;
        Ok((output::parse(&out, code), out, cost))
    }

    async fn check_text(&self, spec: &SpecRef, src: &str, cfg: &CheckConfig) -> Result<Outcome, FormalError> {
        match cfg.mode {
            CheckMode::Bounded(_) => {}
            CheckMode::Inductive => {
                return Err(FormalError::Unsupported(
                    "TLC checks finite instances; proving an inductive invariant needs Apalache (bind `seams.tla = formal-apalache`)"
                        .into(),
                ));
            }
            CheckMode::Proof => return Err(FormalError::Unsupported("TLC does not prove theorems; use lean_build for Proof mode".into())),
            CheckMode::Trace => return Err(FormalError::Unsupported("Trace mode validates a recorded log; use spec_trace".into())),
        }
        let bound = model::depth_bound(cfg.mode);
        let extra: Vec<&str> = if bound.is_some() { vec![model::BOUND_OP] } else { vec![] };
        let cfg_text = model::render_cfg(cfg, &extra)?;
        let spec_hash = Hash::of_json(&(src, &cfg_text));
        let mut work = WorkDir::next_to(&self.fs, spec).await?;
        let module_file = work.write(&format!("{}.tla", spec.module), src).await?;
        let main = match bound {
            Some(n) => work.write(&format!("{}.tla", model::BOUND_MODULE), &model::bound_module(&spec.module, n)).await?,
            None => module_file,
        };
        let cfg_file = work.write("aho.cfg", &cfg_text).await?;
        let result = self.run(&work, &main, &cfg_file, spec.dir(), cfg.budget_ms).await;
        work.close().await?;
        let (res, out, cost) = result?;
        Ok(outcome(res, &out, cfg.mode, spec_hash, cost, |what, _| format!("{} of {}", what.describe(), spec.module)))
    }
}

/// Runs to completion, returning `(exit code, stdout+stderr)`.
async fn run_process(
    proc: &SubprocessHandle,
    argv: Vec<String>,
    cwd: &Path,
    budget_ms: u64,
    what: &str,
) -> Result<(i32, String), FormalError> {
    let spec = SpawnSpec { argv, cwd: Some(cwd.display().to_string()), env: Vec::new(), stdin: None, timeout_ms: Some(budget_ms.max(1)) };
    let child = proc.0.spawn(spec).await.map_err(|e| proc_error(e, what, budget_ms))?;
    let mut bytes = Vec::new();
    let mut cursor = 0;
    loop {
        let (chunk, next, done) = child.output(cursor);
        bytes.extend_from_slice(&chunk);
        cursor = next;
        if done {
            break;
        }
        child.readable(cursor).await;
    }
    let exit = child.wait().await.map_err(|e| proc_error(e, what, budget_ms))?;
    Ok((exit.code, String::from_utf8_lossy(&bytes).into_owned()))
}

fn proc_error(e: ProcError, what: &str, budget_ms: u64) -> FormalError {
    match e {
        ProcError::NotFound(p) => FormalError::ToolMissing { tool: format!("{what}: `{p}`"), fix: INSTALL.into() },
        ProcError::Timeout => FormalError::Timeout { after_ms: budget_ms },
        other => FormalError::Tool(format!("{what} could not run: {other}")),
    }
}

/// Parsed TLC result → the seam's closed `Outcome`.
fn outcome(
    res: TlcResult,
    out: &str,
    mode: CheckMode,
    spec_hash: Hash,
    cost: u64,
    violated: impl Fn(&Violation, &[TraceState]) -> String,
) -> Outcome {
    let tail = aho_formal::tail(out, OUTPUT_TAIL);
    match res {
        TlcResult::Pass { depth, .. } => Outcome {
            verdict: Verdict::Pass,
            mode: match (mode, depth) {
                (CheckMode::Bounded(0), Some(d)) => CheckMode::Bounded(d),
                (m, _) => m,
            },
            counterexample: None,
            goals: Vec::new(),
            diagnostics: Vec::new(),
            spec_hash,
            cost_ms: cost,
            output: tail,
        },
        TlcResult::Violation { what, trace } => {
            let cx = Counterexample { states: trace.iter().map(TraceState::to_value).collect(), violated: violated(&what, &trace) };
            Outcome {
                verdict: Verdict::Fail { counterexample: cx.hash() },
                mode,
                counterexample: Some(cx),
                goals: Vec::new(),
                diagnostics: Vec::new(),
                spec_hash,
                cost_ms: cost,
                output: tail,
            }
        }
        TlcResult::ParseError(msg) => Outcome::unknown(format!("parse error: {msg}"), mode, spec_hash, cost, tail),
        TlcResult::Error { message, .. } => Outcome::unknown(format!("tlc error: {message}"), mode, spec_hash, cost, tail),
    }
}

impl Formal for Tlc {
    fn kind(&self) -> FormalKind {
        FormalKind::Tla
    }

    fn check<'a>(&'a self, spec: &'a SpecRef, cfg: &'a CheckConfig) -> BoxFuture<'a, Result<Outcome, FormalError>> {
        Box::pin(async move {
            let src = read_spec(&self.fs, spec).await?;
            self.check_text(spec, &src, cfg).await
        })
    }

    fn trace_validate<'a>(&'a self, spec: &'a SpecRef, trace: &'a [Value]) -> BoxFuture<'a, Result<Outcome, FormalError>> {
        Box::pin(async move {
            let src = read_spec(&self.fs, spec).await?;
            for needed in ["Init", "vars"] {
                if !model::defines(&src, needed) {
                    return Err(FormalError::Invalid(format!(
                        "{} defines no `{needed}`, which trace validation needs (Init, vars, TraceStep(e)); nothing was checked",
                        spec.module
                    )));
                }
            }
            if !model::defines(&src, "TraceStep") {
                return Err(FormalError::Invalid(format!(
                    "{} defines no `TraceStep(e)` mapping a log record to a step, so a log cannot be validated against it",
                    spec.module
                )));
            }
            if spec.module == model::TRACE_MODULE {
                return Err(FormalError::Invalid(format!(
                    "a spec named {} collides with the generated trace module; rename it",
                    model::TRACE_MODULE
                )));
            }
            let constants = self
                .fs
                .0
                .read(&spec.path.with_extension("cfg").display().to_string(), None)
                .await
                .map(|r| model::constant_sections(&r.text))
                .unwrap_or_default();
            let module = model::trace_module(&spec.module, trace)?;
            let cfg = model::trace_cfg(&constants, model::defines(&src, "TraceInv"));
            let spec_hash = Hash::of_json(&(&src, &module, &cfg));
            let mut work = WorkDir::next_to(&self.fs, spec).await?;
            let main = work.write(&format!("{}.tla", model::TRACE_MODULE), &module).await?;
            let cfg_file = work.write(&format!("{}.cfg", model::TRACE_MODULE), &cfg).await?;
            let result = self.run(&work, &main, &cfg_file, spec.dir(), self.cfg.budget_ms).await;
            work.close().await?;
            let (res, out, cost) = result?;
            Ok(outcome(res, &out, CheckMode::Trace, spec_hash, cost, |what, states| {
                let idx = states.last().and_then(|s| s.var(model::TRACE_INDEX)).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(1);
                let rec = trace.get(idx.saturating_sub(1));
                let event = rec
                    .map(|r| {
                        format!(
                            "{} (entry {})",
                            r.get("action").and_then(Value::as_str).unwrap_or("?"),
                            r.get("id").map(|v| v.to_string()).unwrap_or_else(|| "?".into())
                        )
                    })
                    .unwrap_or_else(|| "?".into());
                match what {
                    Violation::Deadlock => format!("trace event {idx}, {event}, is not a step of {}", spec.module),
                    other => format!("{} of {} after trace event {}", other.describe(), spec.module, idx.saturating_sub(1)),
                }
            }))
        })
    }

    fn prove<'a>(&'a self, module: &'a str) -> BoxFuture<'a, Result<Outcome, FormalError>> {
        Box::pin(async move { Err(FormalError::Unsupported(format!("TLC does not prove theorems; build `{module}` with lean_build"))) })
    }

    fn translate<'a>(&'a self, krate: &'a str, _fns: &'a [String]) -> BoxFuture<'a, Result<Translation, FormalError>> {
        Box::pin(async move {
            Err(FormalError::Unsupported(format!("TLC does not translate Rust; translate `{krate}` with lean_translate (Aeneas)")))
        })
    }

    fn mutate<'a>(&'a self, spec: &'a SpecRef, cfg: &'a CheckConfig) -> BoxFuture<'a, Result<MutationReport, FormalError>> {
        Box::pin(async move {
            let src = read_spec(&self.fs, spec).await?;
            let started = Instant::now();
            let base = self.check_text(spec, &src, cfg).await?;
            if base.verdict != Verdict::Pass {
                return Err(FormalError::Invalid(format!(
                    "the unmutated {} does not pass ({}), so mutants prove nothing; fix the spec or its config first",
                    spec.module,
                    verdict_text(&base)
                )));
            }
            // A mutant that drops the guard keeping the model finite explores forever; it gets ten
            // times what the original took (plus JVM start-up) before it counts as Unknown{timeout}.
            let per_mutant = CheckConfig { budget_ms: (base.cost_ms.saturating_mul(10) + 5_000).min(cfg.budget_ms), ..cfg.clone() };
            let mut report = MutationReport::default();
            for m in mutate::mutants(&src, &cfg.next).into_iter().take(self.cfg.max_mutants) {
                let o = match self.check_text(spec, &m.source, &per_mutant).await {
                    Ok(o) => o,
                    Err(FormalError::Timeout { after_ms }) => Outcome::unknown(
                        format!("timeout: {after_ms} ms (the mutant's state space outgrew ten times the original's)"),
                        cfg.mode,
                        Hash::of(m.source.as_bytes()),
                        per_mutant.budget_ms,
                        String::new(),
                    ),
                    Err(e) => return Err(e),
                };
                let violated = o.counterexample.as_ref().map(|c| c.violated.clone());
                report.mutants.push(MutantOutcome {
                    name: m.name,
                    description: m.description,
                    caught: matches!(o.verdict, Verdict::Fail { .. }),
                    verdict: o.verdict,
                    violated,
                });
            }
            report.cost_ms = started.elapsed().as_millis() as u64;
            Ok(report)
        })
    }
}

fn verdict_text(o: &Outcome) -> String {
    match &o.verdict {
        Verdict::Pass => "Pass".into(),
        Verdict::Fail { .. } => format!("Fail: {}", o.counterexample.as_ref().map(|c| c.violated.as_str()).unwrap_or("")),
        Verdict::Unknown { reason } => format!("Unknown: {reason}"),
        Verdict::Stale => "Stale".into(),
        Verdict::Vacuous => "Vacuous".into(),
    }
}
