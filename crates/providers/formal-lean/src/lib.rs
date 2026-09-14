//! `13` — Lean 4 as the proof backend of the Formal seam. Artifacts go through the bound world's
//! `FsWrite` and lake/charon/aeneas run through its `Subprocess`, so both sides see one world.
//!
//! * `prove(module)` runs `lake build [module]` in `formal.lean.project` and turns the output
//!   into goals (`unsolved goals` with hypotheses and target) and diagnostics. A build that
//!   succeeds only because a declaration uses `sorry` proves nothing and is not `Pass`.
//! * `translate(crate, fns)` is the Aeneas step: `charon cargo --preset=aeneas` to LLBC, then
//!   `aeneas -backend lean` into the project; the result is bound to the translation hash.
//! * `mutate` rewrites operators in a file's definitions and re-checks it with
//!   `lake env lean`; a mutant is caught when a proof breaks.

#![forbid(unsafe_code)]

pub mod mutate;
pub mod output;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use aho_cap::*;
use aho_config::{ConfigKey, Loadout, Ty};
use aho_formal::{
    CheckConfig, CheckMode, Formal, FormalError, FormalHandle, FormalKind, MutantOutcome, MutationReport, Outcome, SpecRef, Translation,
    Verdict,
};
use aho_fs::{FsWrite, ScratchDir};
use aho_proc::{ProcError, SpawnSpec, SubprocessHandle};
use aho_types::{BoxFuture, Hash, Value};
use serde_json::json;

pub use aho_formal::Goal;

use crate::output::LeanReport;

pub const INSTALL: &str = "aho doctor formal --install lean";
const OUTPUT_TAIL: usize = 4000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeanConfig {
    /// The Lake project (directory with `lakefile.lean` / `lakefile.toml`).
    pub project: PathBuf,
    pub lake: String,
    pub charon: String,
    pub aeneas: String,
    /// Where Aeneas output goes, relative to the project.
    pub translations: String,
    pub budget_ms: u64,
    pub max_mutants: usize,
}

impl Config for LeanConfig {
    const PREFIX: &'static str = "formal.lean";
    fn keys() -> Vec<ConfigKey> {
        vec![
            ConfigKey::new(
                "formal.lean.project",
                Ty::String,
                None,
                "Lake project the proofs live in (the directory holding lakefile.lean or lakefile.toml).",
            ),
            ConfigKey::new(
                "formal.lean.lake",
                Ty::String,
                Some(json!("lake")),
                "Lake launcher: a path, or a name on the world's PATH (elan installs it).",
            ),
            ConfigKey::new("formal.lean.charon", Ty::String, Some(json!("charon")), "Charon launcher (Rust → LLBC) for lean_translate."),
            ConfigKey::new("formal.lean.aeneas", Ty::String, Some(json!("aeneas")), "Aeneas launcher (LLBC → Lean) for lean_translate."),
            ConfigKey::new(
                "formal.lean.translations",
                Ty::String,
                Some(json!("Aeneas")),
                "Directory, inside the project, that Aeneas translations are written to.",
            ),
            ConfigKey::new(
                "formal.lean.budget_ms",
                Ty::Int,
                Some(json!(600_000)),
                "Wall-clock budget for one lake/lean/charon/aeneas run.",
            ),
            ConfigKey::new("formal.lean.max_mutants", Ty::Int, Some(json!(16)), "Most mutants one `invariant_mutate` run checks."),
        ]
    }
    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let project =
            l.str("formal.lean.project").filter(|s| !s.is_empty()).ok_or_else(|| ConfigError::MissingKey("formal.lean.project".into()))?;
        let int = |k: &str, d: u64| match l.get(k) {
            None => Ok(d),
            Some(v) => v.as_u64().ok_or(ConfigError::Type { key: k.into(), expected: "a non-negative integer" }),
        };
        Ok(LeanConfig {
            project: PathBuf::from(project),
            lake: cfg_str(l, "formal.lean.lake", "lake"),
            charon: cfg_str(l, "formal.lean.charon", "charon"),
            aeneas: cfg_str(l, "formal.lean.aeneas", "aeneas"),
            translations: cfg_str(l, "formal.lean.translations", "Aeneas"),
            budget_ms: int("formal.lean.budget_ms", 600_000)?,
            max_mutants: int("formal.lean.max_mutants", 16)? as usize,
        })
    }
}

/// The world's filesystem and process spawner: artifacts and lake share one world (07, 13).
pub struct LeanDeps {
    pub proc: SubprocessHandle,
    pub fs: FsWrite,
}

impl Deps for LeanDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Subprocess, HandleKind::FsWrite];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(LeanDeps { proc: r.handle::<SubprocessHandle>()?, fs: r.handle::<FsWrite>()? })
    }
}

#[derive(Clone)]
pub struct FormalLean {
    inner: Arc<Lean>,
}

impl Capability for FormalLean {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = LeanDeps;
    type Config = LeanConfig;
    fn build(deps: LeanDeps, cfg: LeanConfig) -> Result<Self, BuildError> {
        // The project is checked in the bound world when a proof runs, not on the host here.
        Ok(FormalLean { inner: Arc::new(Lean::new(deps.proc, deps.fs, cfg)) })
    }
    fn register(&self, r: &mut Registrar<'_>) {
        r.service(aho_formal::service_name(FormalKind::Lean), FormalHandle(self.inner.clone()));
    }
}

declare_capability!(
    FormalLean,
    "formal-lean",
    kind = Kind::Provider,
    provides = [aho_formal::LEAN_SEAM],
    requires = [
        Requirement::AnyOf(&[Requirement::Binary("lake"), Requirement::Binary("aeneas")]),
        Requirement::Setting { key: "formal.lean.project", pred: Pred::Set },
    ],
    tools = &[],
    unlock = Fix::cli(INSTALL),
    enforced_by = "the bound world (artifact IO uses FsWrite; lake/charon/aeneas run through Subprocess)",
    doc = "Lean 4 proofs via lake; Rust → Lean translation via Charon + Aeneas; definition mutants (13)."
);

/// The Lean backend, usable without a composition (tests, `aho doctor`).
pub struct Lean {
    proc: SubprocessHandle,
    fs: FsWrite,
    cfg: LeanConfig,
}

fn tool_err(what: &str, path: &Path, e: aho_fs::FsError) -> FormalError {
    FormalError::Tool(format!("could not {what} {}: {e}", path.display()))
}

impl Lean {
    /// A relative `formal.lean.project` resolves against the world's workspace root.
    pub fn new(proc: SubprocessHandle, fs: FsWrite, mut cfg: LeanConfig) -> Lean {
        if cfg.project.is_relative() {
            cfg.project = Path::new(&fs.0.root()).join(&cfg.project);
        }
        Lean { proc, fs, cfg }
    }

    /// The project's workspace-relative path, after checking it is a directory in the world.
    async fn project_rel(&self) -> Result<String, FormalError> {
        let project = &self.cfg.project;
        match self.fs.0.stat(&project.display().to_string()).await {
            Ok(s) if s.is_dir => Ok(s.path),
            _ => Err(FormalError::Invalid(format!(
                "formal.lean.project points at {}, which is not a directory in the bound world, so there is nothing to build. Run `lake new <name>` there or `aho set formal.lean.project <dir>`.",
                project.display()
            ))),
        }
    }

    async fn read(&self, path: &Path) -> Option<String> {
        self.fs.0.read(&path.display().to_string(), None).await.ok().map(|r| r.text)
    }

    async fn scratch(&self, prefix: &str, what: &str) -> Result<Box<dyn ScratchDir>, FormalError> {
        self.fs.0.scratch_dir(&self.cfg.project.display().to_string(), prefix).await.map_err(|e| {
            FormalError::Tool(format!("could not create a scratch directory in {}: {e}; nothing was {what}", self.cfg.project.display()))
        })
    }

    async fn run(&self, argv: Vec<String>, cwd: &Path, what: &str) -> Result<(i32, String, u64), FormalError> {
        let started = Instant::now();
        let spec = SpawnSpec {
            argv,
            cwd: Some(cwd.display().to_string()),
            env: Vec::new(),
            stdin: None,
            timeout_ms: Some(self.cfg.budget_ms.max(1)),
        };
        let child = self.proc.0.spawn(spec).await.map_err(|e| proc_error(e, what, self.cfg.budget_ms))?;
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
        let exit = child.wait().await.map_err(|e| proc_error(e, what, self.cfg.budget_ms))?;
        Ok((exit.code, String::from_utf8_lossy(&bytes).into_owned(), started.elapsed().as_millis() as u64))
    }

    /// Goals named after their declaration (`fold_append (case h)`), read from the source.
    async fn name_goals(&self, report: &LeanReport) -> Vec<Goal> {
        let mut goals = Vec::with_capacity(report.goals.len());
        for (i, g) in &report.goals {
            let d = &report.diagnostics[*i];
            let decl = self.read(&self.cfg.project.join(&d.file)).await.and_then(|src| output::enclosing_decl(&src, d.line));
            let at = format!("{}:{}:{}", d.file, d.line, d.col);
            let base = decl.map(|n| format!("{n} ({at})")).unwrap_or(at);
            let name = if g.name.is_empty() { base } else { format!("{base} case {}", g.name) };
            goals.push(Goal { name, hypotheses: g.hypotheses.clone(), target: g.target.clone() });
        }
        goals
    }

    async fn outcome(&self, code: i32, out: &str, cost: u64, spec_hash: Hash) -> Outcome {
        let report = output::parse(out);
        let goals = self.name_goals(&report).await;
        let errors: Vec<&aho_formal::Diagnostic> = report.errors().collect();
        let verdict = if !goals.is_empty() {
            let names: Vec<&str> = goals.iter().map(|g| g.name.as_str()).collect();
            Verdict::Unknown { reason: format!("unproved: {} open goal(s) in {}", goals.len(), names.join(", ")) }
        } else if let Some(e) = errors.first() {
            Verdict::Unknown { reason: format!("build error: {}:{}:{}: {}", e.file, e.line, e.col, e.message.lines().next().unwrap_or("")) }
        } else if !report.sorries.is_empty() {
            let at: Vec<String> = report
                .sorries
                .iter()
                .map(|i| {
                    let d = &report.diagnostics[*i];
                    format!("{}:{}", d.file, d.line)
                })
                .collect();
            Verdict::Unknown { reason: format!("incomplete: `sorry` at {}; it builds but proves nothing", at.join(", ")) }
        } else if code != 0 {
            Verdict::Unknown { reason: format!("lake exited with code {code} without a Lean error; see the output") }
        } else {
            Verdict::Pass
        };
        Outcome {
            verdict,
            mode: CheckMode::Proof,
            counterexample: None,
            goals,
            diagnostics: report.diagnostics,
            spec_hash,
            cost_ms: cost,
            output: aho_formal::tail(out, OUTPUT_TAIL),
        }
    }

    /// `(project-relative path, text)` of every `.lean` file a module (or the whole project) covers.
    async fn sources(&self, project_rel: &str, module: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if !module.is_empty() {
            let rel: PathBuf = module.split('.').collect::<PathBuf>().with_extension("lean");
            if let Some(t) = self.read(&self.cfg.project.join(&rel)).await {
                out.push((rel.display().to_string(), t));
            }
            return out;
        }
        self.lean_files(project_rel, project_rel, &mut out).await;
        out
    }

    /// Every `.lean` file under workspace-relative `dir`, keyed relative to `root`; skips hidden
    /// entries and lake's `build` output.
    async fn lean_files(&self, root: &str, dir: &str, out: &mut Vec<(String, String)>) {
        let mut stack = vec![dir.to_string()];
        while let Some(d) = stack.pop() {
            let Ok(entries) = self.fs.0.list(&d).await else { continue };
            for e in entries {
                let name = e.path.rsplit('/').next().unwrap_or_default();
                if name.starts_with('.') || name == "build" {
                    continue;
                }
                if e.is_dir {
                    stack.push(e.path);
                } else if name.ends_with(".lean")
                    && let Ok(r) = self.fs.0.read(&e.path, None).await
                {
                    let rel = e.path.strip_prefix(root).map(|r| r.trim_start_matches('/')).unwrap_or(&e.path);
                    out.push((rel.to_string(), r.text));
                }
            }
        }
        out.sort();
    }
}

fn proc_error(e: ProcError, what: &str, budget_ms: u64) -> FormalError {
    match e {
        ProcError::NotFound(p) => FormalError::ToolMissing { tool: format!("{what}: `{p}`"), fix: INSTALL.into() },
        ProcError::Timeout => FormalError::Timeout { after_ms: budget_ms },
        other => FormalError::Tool(format!("{what} could not run: {other}")),
    }
}

/// `my-crate` → `MyCrate` (the Lean namespace Aeneas output is filed under).
pub fn camel(s: &str) -> String {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut cs = p.chars();
            cs.next().map(|c| c.to_ascii_uppercase().to_string() + cs.as_str()).unwrap_or_default()
        })
        .collect()
}

impl Formal for Lean {
    fn kind(&self) -> FormalKind {
        FormalKind::Lean
    }

    fn check<'a>(&'a self, spec: &'a SpecRef, cfg: &'a CheckConfig) -> BoxFuture<'a, Result<Outcome, FormalError>> {
        Box::pin(async move {
            match cfg.mode {
                CheckMode::Proof => self.prove(&spec.module).await,
                other => Err(FormalError::Unsupported(format!(
                    "Lean proves theorems (Proof mode); {other:?} is a model-checking mode, use spec_check on a TLA+ spec"
                ))),
            }
        })
    }

    fn trace_validate<'a>(&'a self, spec: &'a SpecRef, _trace: &'a [Value]) -> BoxFuture<'a, Result<Outcome, FormalError>> {
        Box::pin(async move {
            Err(FormalError::Unsupported(format!(
                "Lean does not validate traces; validate the log against a TLA+ spec with spec_trace ({} is a Lean module)",
                spec.module
            )))
        })
    }

    fn prove<'a>(&'a self, module: &'a str) -> BoxFuture<'a, Result<Outcome, FormalError>> {
        Box::pin(async move {
            if !module.is_empty() && !module.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_alphanumeric() || c == '_')) {
                return Err(FormalError::Invalid(format!("`{module}` is not a Lean module name (e.g. Aho.Fold); nothing was built")));
            }
            let project_rel = self.project_rel().await?;
            let spec_hash = Hash::of_json(&self.sources(&project_rel, module).await);
            let mut argv = vec![self.cfg.lake.clone(), "build".into()];
            if !module.is_empty() {
                argv.push(module.to_string());
            }
            let (code, out, cost) = self.run(argv, &self.cfg.project, "lake").await?;
            Ok(self.outcome(code, &out, cost, spec_hash).await)
        })
    }

    fn translate<'a>(&'a self, krate: &'a str, fns: &'a [String]) -> BoxFuture<'a, Result<Translation, FormalError>> {
        Box::pin(async move {
            let krate_dir = PathBuf::from(krate);
            if !self.is_file(&krate_dir.join("Cargo.toml")).await {
                return Err(FormalError::Invalid(format!("{krate} has no Cargo.toml; lean_translate takes the crate's directory")));
            }
            let project_rel = self.project_rel().await?;
            let mut scratch = self.scratch(".aho-charon-", "translated").await?;
            let result = self.translate_in(scratch.path(), &project_rel, &krate_dir, fns).await;
            scratch.close().await.map_err(|e| FormalError::Tool(format!("could not remove the Charon scratch directory: {e}")))?;
            result
        })
    }

    fn mutate<'a>(&'a self, spec: &'a SpecRef, _cfg: &'a CheckConfig) -> BoxFuture<'a, Result<MutationReport, FormalError>> {
        Box::pin(async move {
            let started = Instant::now();
            let src = self
                .fs
                .0
                .read(&spec.path.display().to_string(), None)
                .await
                .map(|r| r.text)
                .map_err(|e| FormalError::Invalid(format!("could not read {}: {e}; nothing was mutated", spec.path.display())))?;
            self.project_rel().await?;
            let mut scratch = self.scratch(".aho-lean-", "mutated").await?;
            let result = self.mutate_in(scratch.path(), spec, &src).await;
            scratch.close().await.map_err(|e| FormalError::Tool(format!("could not remove the Lean scratch directory: {e}")))?;
            let mut report = result?;
            report.cost_ms = started.elapsed().as_millis() as u64;
            Ok(report)
        })
    }
}

impl Lean {
    async fn is_file(&self, path: &Path) -> bool {
        self.fs.0.stat(&path.display().to_string()).await.is_ok_and(|s| !s.is_dir)
    }

    async fn translate_in(&self, scratch: &str, project_rel: &str, krate_dir: &Path, fns: &[String]) -> Result<Translation, FormalError> {
        let name = krate_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "crate".into());
        let module_ns = camel(&name);
        let llbc = Path::new(scratch).join(format!("{name}.llbc"));
        let mut argv =
            vec![self.cfg.charon.clone(), "cargo".into(), "--preset=aeneas".into(), "--dest-file".into(), llbc.display().to_string()];
        for f in fns {
            argv.push("--start-from".into());
            argv.push(if f.contains("::") { f.clone() } else { format!("{}::{f}", name.replace('-', "_")) });
        }
        let (code, out, _) = self.run(argv, krate_dir, "charon").await?;
        if code != 0 || !self.is_file(&llbc).await {
            return Err(FormalError::Tool(format!(
                "charon could not extract {name} (exit {code}); Aeneas covers safe Rust without raw pointers or unsafe. Output:\n{}",
                aho_formal::tail(&out, 2000)
            )));
        }
        let dest = self.cfg.project.join(&self.cfg.translations).join(&module_ns);
        let dest_s = dest.display().to_string();
        self.fs.0.create_dir(&dest_s).await.map_err(|e| tool_err("create", &dest, e))?;
        let dest_rel = self.fs.0.stat(&dest_s).await.map_err(|e| tool_err("open", &dest, e))?.path;
        let argv = vec![
            self.cfg.aeneas.clone(),
            "-backend".into(),
            "lean".into(),
            "-namespace".into(),
            module_ns.clone(),
            "-dest".into(),
            dest_s,
            llbc.display().to_string(),
        ];
        let (code, out, _) = self.run(argv, &self.cfg.project, "aeneas").await?;
        if code != 0 {
            return Err(FormalError::Tool(format!(
                "aeneas could not translate {name} (exit {code}). Output:\n{}",
                aho_formal::tail(&out, 2000)
            )));
        }
        let mut files = Vec::new();
        self.lean_files(project_rel, &dest_rel, &mut files).await;
        let Some((main, _)) = files.iter().find(|(p, _)| !p.ends_with("Template.lean")).or(files.first()) else {
            return Err(FormalError::Tool(format!("aeneas finished but wrote no .lean file under {}", dest.display())));
        };
        let module = main.trim_end_matches(".lean").replace(['/', '\\'], ".");
        Ok(Translation { module, path: self.cfg.project.join(main), hash: Hash::of_json(&files) })
    }

    async fn mutate_in(&self, scratch: &str, spec: &SpecRef, src: &str) -> Result<MutationReport, FormalError> {
        let file_name = spec.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Mutant.lean".into());
        let path = Path::new(scratch).join(&file_name);
        let check = |text: String| {
            let path = path.clone();
            async move {
                self.fs.0.write(&path.display().to_string(), &text, None).await.map_err(|e| tool_err("write", &path, e))?;
                let argv = vec![self.cfg.lake.clone(), "env".into(), "lean".into(), path.display().to_string()];
                let (code, out, _) = self.run(argv, &self.cfg.project, "lake").await?;
                Ok::<_, FormalError>((code, output::parse(&out)))
            }
        };
        let (code, base) = check(src.to_string()).await?;
        if code != 0 || base.errors().next().is_some() || !base.sorries.is_empty() {
            return Err(FormalError::Invalid(format!(
                "{} does not check cleanly as it is (exit {code}, {} error(s), {} sorry), so mutants prove nothing; fix it first",
                spec.path.display(),
                base.errors().count(),
                base.sorries.len()
            )));
        }
        let mut report = MutationReport::default();
        for m in mutate::mutants(src).into_iter().take(self.cfg.max_mutants) {
            let (_, r) = check(m.source.clone()).await?;
            let proof_errors: Vec<&aho_formal::Diagnostic> = r.errors().filter(|d| mutate::in_proof(&m.source, d.line)).collect();
            let (caught, verdict, violated) = if let Some(d) = proof_errors.first() {
                let decl = output::enclosing_decl(&m.source, d.line).unwrap_or_else(|| format!("line {}", d.line));
                (
                    true,
                    Verdict::Fail { counterexample: Hash::of_json(&(&m.name, &d.message)) },
                    Some(format!("{decl}: {}", d.message.lines().next().unwrap_or(""))),
                )
            } else if let Some(d) = r.errors().next() {
                (false, Verdict::Unknown { reason: format!("ill-typed mutant: {}", d.message.lines().next().unwrap_or("")) }, None)
            } else {
                (false, Verdict::Pass, None)
            };
            report.mutants.push(MutantOutcome { name: m.name, description: m.description, caught, verdict, violated });
        }
        Ok(report)
    }
}
