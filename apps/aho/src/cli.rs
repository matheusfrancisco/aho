//! The verb set as clap types. Every verb here is also a `caps.*` / `session.*` rpc method
//! or a projection over a log; none exists in this client only.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "aho", version, about = "aho: a modular coding harness. It shows what exists, what is on, why, and how to change it.")]
pub struct Cli {
    #[command(flatten)]
    pub global: Global,
    #[command(subcommand)]
    pub verb: Verb,
}

fn key_value(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .filter(|(k, _)| !k.trim().is_empty())
        .map(|(k, v)| (k.trim().to_string(), v.to_string()))
        .ok_or_else(|| format!("`{s}` is not KEY=VALUE"))
}

#[derive(Args, Debug, Clone, Default)]
pub struct Global {
    /// Set a config key for this invocation (the Arg layer), e.g. `--set model.name=gpt-4.1`. Repeatable.
    #[arg(long = "set", value_name = "KEY=VALUE", global = true, value_parser = key_value)]
    pub sets: Vec<(String, String)>,
    /// Disable a capability for this invocation; always wins, even over a broken config. Repeatable.
    #[arg(long = "disable", value_name = "ID", global = true)]
    pub disable: Vec<String>,
    /// Named profile layer `~/.aho/profiles/<name>.toml`.
    #[arg(long, value_name = "NAME", global = true)]
    pub profile: Option<String>,
    /// Project directory (default: the current directory).
    #[arg(long, value_name = "DIR", global = true)]
    pub project: Option<PathBuf>,
    /// Add a saved loadout (`aho loadout save`) as a layer.
    #[arg(long, value_name = "NAME", global = true)]
    pub loadout: Option<String>,
    /// Answer `apply? [y/N]` with yes.
    #[arg(short = 'y', long, global = true)]
    pub yes: bool,
}

#[derive(Subcommand, Debug)]
pub enum Verb {
    /// Resolve and print the capability table, config, diagnostics, and hashes. Opens no log.
    Plan {
        /// A proposed layer; prints the state diff it would cause.
        #[arg(long, value_name = "FILE", conflicts_with = "lock")]
        patch: Option<PathBuf>,
        /// Write aho.lock with the resolved plan.
        #[arg(long)]
        lock: bool,
        /// Any diagnostic at error severity aborts (exit 2).
        #[arg(long)]
        strict: bool,
        #[arg(long)]
        json: bool,
    },
    /// The tech tree: every capability, its state, and the one fix for each locked node.
    Tree {
        id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// One uniform card per capability.
    Inventory {
        /// provider tool skill mcp recipe media surface panel hook theme template
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Explain where a setting's value came from, or why a capability is in its state.
    Explain {
        /// A config key (`policy.preset`) or a capability id (`tool-bash`).
        subject: String,
        /// Show what the agent saw at entry N of a session log.
        #[arg(long, value_name = "N")]
        at: Option<u64>,
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Explain why a capability is Active, Locked, Broken, Unlocked, or Disabled.
    Why {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Generated catalogs.
    Catalog {
        #[command(subcommand)]
        what: CatalogWhat,
    },
    /// Turn a capability on (plan → diff → apply).
    Enable { id: String },
    /// Turn a capability off (plan → diff → apply).
    Disable { id: String },
    /// Let a capability's classes pass even where the preset would ask (plan → diff → apply).
    Allow { id: String },
    /// Write a setting into the most specific config file (plan → diff → apply).
    Set { key: String, value: String },
    /// Switch the policy preset: read-only | workspace | dev | lab | full | spec.
    Preset { name: String },
    /// Frozen resolved tables under ~/.aho/loadouts/.
    Loadout {
        #[command(subcommand)]
        sub: LoadoutVerb,
    },
    /// Write aho.lock; `aho run --lock` then refuses a plan that differs.
    Lock,
    /// Run the agent: one prompt (`-p`), stdin, or an interactive loop.
    Run(RunArgs),
    /// Session logs.
    Session {
        #[command(subcommand)]
        sub: SessionVerb,
    },
    /// A session's sandbox network: `status`, `allow <dest>`, `map HOST:GUEST`.
    Net {
        #[command(subcommand)]
        sub: NetVerb,
        /// The session (id or unique prefix; default: the most recent).
        #[arg(long, value_name = "ID", global = true)]
        session: Option<String>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// A session's sandbox: `status`, or `destroy` it (the next turn creates a fresh one).
    Sandbox {
        #[command(subcommand)]
        sub: SandboxVerb,
        #[arg(long, value_name = "ID", global = true)]
        session: Option<String>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Copy a file or directory out of the session's sandbox (or ssh world) to the host;
    /// nothing on the host is overwritten.
    Export {
        /// A path in the sandbox workspace, e.g. `./src`.
        src: String,
        /// The host destination: a new path, or an existing directory to copy into.
        dst: PathBuf,
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
    /// VM leases: what a session's machines may use, granted and revoked by the operator.
    Lease {
        #[command(subcommand)]
        sub: LeaseVerb,
        #[arg(long, value_name = "ID", global = true)]
        session: Option<String>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// The operator's VM media library: immutable, hashed images recipes are built from.
    Media {
        #[command(subcommand)]
        sub: MediaVerb,
    },
    /// Workspace checkpoints on a session's transcript.
    Checkpoints {
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
    /// Go back to a checkpoint: chat and code (default), chat only, or code only.
    Rewind {
        /// `c2` or `2`.
        checkpoint: String,
        /// Fork the conversation only; the workspace is untouched.
        #[arg(long, conflicts_with = "code")]
        chat: bool,
        /// Restore the workspace only, on the same branch; the model is told.
        #[arg(long)]
        code: bool,
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
    /// An operator shell in the workspace, or one logged command (`aho shell -- cargo test`).
    Shell {
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Plan, policy, active capabilities (loudest first), open calls, and the latest session.
    Status {
        /// Also list diagnostics, every non-active capability with its fix, and preset notes.
        #[arg(long)]
        loud: bool,
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Serve the NDJSON rpc protocol on stdio, or on a unix socket.
    Rpc {
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
    /// Spec mode: the agent works with you on spec/ only.
    Spec {
        #[command(subcommand)]
        sub: Option<SpecVerb>,
    },
    /// Named credentials (`cred:<name>`), kept in ~/.aho/config.toml.
    Credential {
        #[command(subcommand)]
        sub: CredentialVerb,
    },
    /// Sign in to a model provider; credentials refresh automatically during inference.
    Login(LoginArgs),
    /// Remove a stored account login (does not revoke it at the provider or clear API keys).
    Logout { provider: Option<String> },
    /// Add a runtime capability source.
    Add {
        #[command(subcommand)]
        what: AddWhat,
    },
    /// Write a layer file into the most specific config file (plan → diff → apply), then record
    /// the change in the latest (or named) session's log when no process drives it.
    Apply {
        /// A TOML layer, e.g. `x.toml` with `[policy] preset = "dev"`.
        file: PathBuf,
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
    /// The daemon: one process owns every session of this project; clients attach over a
    /// unix socket. `aho run` goes through it while it serves.
    Serve(ServeArgs),
    /// A thin client of `aho serve`: open or resume a session in the daemon and drive it.
    Attach(AttachArgs),
    /// The terminal UI over a session; `--headless` prints the normalized screen as JSON.
    Tui {
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        #[arg(long)]
        headless: bool,
        /// Keep the whole session on the alternate screen instead of inline in the terminal's
        /// scrollback (the transcript then scrolls with PgUp/PgDn).
        #[arg(long, conflicts_with = "headless")]
        fullscreen: bool,
        #[arg(long, default_value_t = 120)]
        width: u16,
        #[arg(long, default_value_t = 32)]
        height: u16,
    },
    /// The web UI over a session: HTTPS with `--cert/--key` or the pair `aho cert` wrote;
    /// plain HTTP only on loopback with `--insecure-loopback` (development).
    Web {
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        #[arg(long, default_value = "127.0.0.1:8787", value_name = "ADDR")]
        bind: String,
        #[arg(long, value_name = "PEM", requires = "key")]
        cert: Option<PathBuf>,
        #[arg(long, value_name = "PEM", requires = "cert")]
        key: Option<PathBuf>,
        /// Serve plain HTTP on a loopback address. Development only: anyone on this machine can reach it.
        #[arg(long, conflicts_with_all = ["cert", "key"])]
        insecure_loopback: bool,
    },
    /// Generate the local TLS pair `aho web` and `aho webhook` serve with, under ~/.aho/tls/.
    Cert {
        /// Another name or IP the certificate should cover (localhost, 127.0.0.1, ::1 always are). Repeatable.
        #[arg(long = "host", value_name = "NAME")]
        hosts: Vec<String>,
        /// Replace an existing pair.
        #[arg(long)]
        force: bool,
    },
    /// The GitHub webhook listener: signed deliveries become prompts to one session.
    Webhook {
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        /// Plain HTTP on a loopback bind (behind a tunnel that terminates TLS). Development only.
        #[arg(long)]
        insecure_loopback: bool,
    },
    /// Agent Client Protocol over stdio: editors (Zed, …) drive aho sessions.
    Acp,
    /// Self-dev reload: publish this build, smoke it, flip the channels, hand the daemon over.
    Selfdev {
        #[command(subcommand)]
        sub: SelfdevVerb,
    },
    /// Branch a session at entry N; with `--loadout X` the new branch runs under that loadout.
    Fork {
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        #[arg(long, value_name = "N")]
        at: u64,
    },
    /// Checkpoint trees outside the session.
    Checkpoint {
        #[command(subcommand)]
        sub: CheckpointVerb,
    },
    /// Latest verdict per invariant against the current code; a verdict older than the code is Stale.
    /// `show`, `check` and `mutate` run the checkers from here, as the operator.
    Invariants {
        #[command(subcommand)]
        sub: Option<InvariantsVerb>,
        /// The session to read and record into (id or unique prefix; default: the most recent).
        #[arg(long, value_name = "ID", global = true)]
        session: Option<String>,
        #[arg(long, global = true)]
        json: bool,
    },
    /// Ordered readiness checks.
    Doctor {
        #[command(subcommand)]
        what: DoctorWhat,
    },
    /// Capability manifests as the binary declares them.
    Caps {
        #[command(subcommand)]
        sub: CapsVerb,
    },
    /// Schedules the agent set up with `schedule_*` (fired by `aho serve`, `aho tui`, `aho web`).
    Schedule {
        #[command(subcommand)]
        sub: ScheduleVerb,
    },
    /// The `ralph` loop (`workflow_*` tools).
    Ralph {
        #[command(subcommand)]
        sub: RalphVerb,
    },
    /// Session modes.
    PlanMode {
        #[command(subcommand)]
        sub: PlanModeVerb,
    },
    /// Reclaim what dead sessions left behind: worktrees, spill, checkpoint trees, child logs.
    Gc {
        /// List what would be reclaimed, by owning session and age; delete nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum LeaseVerb {
    /// The session's live VM lease.
    Status,
    /// Grant the session a VM lease (logged as the operator).
    Grant {
        /// Machines at once, the sandbox VM included.
        #[arg(long, default_value_t = 2)]
        machines: u32,
        /// Total virtual disk, e.g. 40G.
        #[arg(long, default_value = "40G")]
        disk: String,
        /// Total CPU time (vCPUs × running time), e.g. 6h.
        #[arg(long, default_value = "6h")]
        cpu: String,
        /// Lease duration, e.g. 6h or 2d.
        #[arg(long, default_value = "6h")]
        expires: String,
    },
    /// End the session's lease now: its machines stop, no VM effect runs.
    Revoke,
}

#[derive(Subcommand, Debug)]
pub enum MediaVerb {
    /// Copy an image into the library under an immutable name (read-only, content-hashed).
    Import {
        file: PathBuf,
        /// The catalog name (default: the file name).
        #[arg(long)]
        name: Option<String>,
        /// raw | qcow2 | iso | kernel | initrd | firmware (default: from the extension).
        #[arg(long)]
        kind: Option<String>,
    },
    List {
        #[arg(long)]
        json: bool,
    },
}

/// `aho add recipe`: a VM recipe over catalog media; only typed fields, no QEMU arguments.
#[derive(Args, Debug)]
pub struct RecipeArgs {
    pub name: String,
    /// Base disk image: `media:<name>` (raw or qcow2); new disks are copy-on-write over it.
    #[arg(long, value_name = "MEDIA")]
    pub base: Option<String>,
    /// Installer or live ISO: `media:<name>`.
    #[arg(long, value_name = "MEDIA")]
    pub cdrom: Option<String>,
    #[arg(long, value_name = "MEDIA")]
    pub kernel: Option<String>,
    #[arg(long, value_name = "MEDIA")]
    pub initrd: Option<String>,
    /// UEFI firmware (aarch64 needs one, or a kernel).
    #[arg(long, value_name = "MEDIA")]
    pub firmware: Option<String>,
    /// Kernel command line (with --kernel).
    #[arg(long)]
    pub append: Option<String>,
    /// aarch64 | x86_64 (default: this host's).
    #[arg(long)]
    pub arch: Option<String>,
    /// tcg | kvm | hvf (default: the host's accelerator when the architecture matches).
    #[arg(long)]
    pub accel: Option<String>,
    #[arg(long, default_value_t = 2)]
    pub cpus: u16,
    #[arg(long, default_value = "2G")]
    pub mem: String,
    #[arg(long, default_value = "20G")]
    pub disk: String,
    /// The guest directory tools work in.
    #[arg(long, default_value = "/workspace")]
    pub workspace: String,
    /// Overwrite an existing recipe of that name.
    #[arg(long)]
    pub replace: bool,
}

#[derive(Subcommand, Debug)]
pub enum SandboxVerb {
    /// Created, running, where it lives, its NIC.
    Status,
    /// Stop everything in it and delete its workspace and volume (asks first).
    Destroy {
        /// Do not ask.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum NetVerb {
    /// Subnets, isolation, egress lists, port maps, NICs; live flows when it runs.
    Status,
    /// Let a blocked destination through: a domain, `*.domain`, IP or CIDR, or an isolation
    /// class (`internet`, `host-lan`, `other-sessions`): the unlock a blocked flow names.
    Allow { dest: String },
    /// Forward a host port into the sandbox: `aho net map 8080:3000` (or `8080` for the same port).
    Map {
        /// HOST_PORT:GUEST_PORT.
        spec: String,
        /// The host address to listen on; loopback unless you name another.
        #[arg(long, default_value = "127.0.0.1", value_name = "IP")]
        bind: std::net::IpAddr,
        /// The guest NIC to forward to (a VM's name; the namespace sandbox is `sandbox`).
        #[arg(long, default_value = crate::net::SANDBOX, value_name = "NAME")]
        guest: String,
        #[arg(long)]
        udp: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum CatalogWhat {
    /// Every settable key, with type, default, env var, and doc.
    Config,
}

#[derive(Subcommand, Debug)]
pub enum LoadoutVerb {
    /// Freeze the currently resolved table.
    Save {
        name: String,
    },
    /// Compare two saved loadouts, or one against the current resolution.
    Diff {
        a: String,
        b: Option<String>,
    },
    /// Show what a saved loadout would change; use it with `--loadout <name>`.
    Load {
        name: String,
    },
    List,
}

#[derive(Args, Debug, Default)]
pub struct RunArgs {
    /// The prompt; without it, stdin (piped) or an interactive loop (terminal).
    #[arg(short = 'p', long)]
    pub prompt: Option<String>,
    /// Drive the deterministic scripted model with this JSON script (sets model.provider=scripted).
    #[arg(long, value_name = "FILE")]
    pub script: Option<PathBuf>,
    /// Resume this session (id or unique prefix).
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,
    /// Annotate the transcript with entry ids, principals, and owning capabilities.
    #[arg(long)]
    pub hud: bool,
    /// Log entries as NDJSON on stdout; stderr stays empty unless something fails.
    #[arg(long)]
    pub json: bool,
    /// Refuse to start when the plan differs from aho.lock.
    #[arg(long)]
    pub lock: bool,
    /// Refuse to start unless the plan hash starts with this.
    #[arg(long, value_name = "HASH")]
    pub plan: Option<String>,
    #[arg(long)]
    pub strict: bool,
}

#[derive(Subcommand, Debug)]
pub enum SessionVerb {
    List {
        #[arg(long)]
        json: bool,
    },
    /// The transcript and the catch-up brief.
    Show { id: String },
    /// Hash chain, fold invariants, replay equality, and open calls; `--formal` also validates the
    /// log against aho's own TLA+ specs (every session log is a behavior).
    Verify {
        /// Session id or unique prefix (default: the most recent).
        id: Option<String>,
        #[arg(long)]
        formal: bool,
    },
    /// Every branch of a session's log tree (rewinds fork; nothing is deleted).
    Branches { id: Option<String> },
    /// The difference between the folds of two branch heads (`aho session branches` lists them).
    Diff {
        /// Entry id of one head (`entry N` in `aho session branches`).
        a: u64,
        b: u64,
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
    /// Bring a worktree session's branch `aho/<name>` back into the checked-out branch.
    Merge {
        id: Option<String>,
        /// Merge even though the spec's open questions grew; the reason is logged.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Remove a worktree session's worktree; its branch and log stay.
    Destroy { id: Option<String> },
    /// Search every session's log in this project (the rebuildable index next to the logs).
    /// `terms "a phrase" kind:tool_call tool:bash session:<hex> after:<ms> before:<ms> branch:all limit:N`
    Query {
        query: String,
        #[arg(long)]
        json: bool,
    },
    /// The cross-session index: what it covers and which logs it could not read.
    Index {
        /// Re-read every log in the sessions directory (after logs were edited or copied in).
        #[arg(long)]
        rebuild: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args, Debug, Default, Clone)]
pub struct ServeArgs {
    /// Socket path (default `<project>/.aho/aho.sock`).
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Exit after this many seconds with no clients and no running turns.
    #[arg(long, value_name = "SECS")]
    pub idle_exit: Option<u64>,
    /// Start in the background and return once it serves (log: `<project>/.aho/daemon.log`).
    #[arg(long, conflicts_with = "stop")]
    pub detach: bool,
    /// Ask the daemon on the socket to shut down (turns wind down and are closed in their logs).
    #[arg(long)]
    pub stop: bool,
}

#[derive(Args, Debug, Default, Clone)]
pub struct AttachArgs {
    /// Session id or unique prefix; omitted = a new session.
    pub session: Option<String>,
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// The prompt; without it, stdin (piped) or an interactive loop (terminal).
    #[arg(short = 'p', long)]
    pub prompt: Option<String>,
    /// Wire frames as NDJSON on stdout.
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub hud: bool,
}

#[derive(Subcommand, Debug)]
pub enum SelfdevVerb {
    /// Publish a build, smoke-boot it, flip `current`/`canary`, and reload the daemon into it.
    Reload {
        /// The binary to publish (default: this aho).
        #[arg(long, value_name = "FILE")]
        binary: Option<PathBuf>,
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
        /// The session that asked (continues first after the reload).
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        /// Seconds to wait for the new daemon to signal readiness.
        #[arg(long, default_value_t = 60, value_name = "SECS")]
        timeout: u64,
    },
    /// Channels on disk, what the ledger says, and what a rollback would restore.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Reload the daemon back into the build that ran before the last activation.
    Rollback {
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        #[arg(long, default_value_t = 60, value_name = "SECS")]
        timeout: u64,
    },
}

#[derive(Subcommand, Debug)]
pub enum CheckpointVerb {
    /// Turn a checkpoint into a real commit on a branch of your repository.
    Export {
        /// `c2` or `2`.
        checkpoint: String,
        #[arg(long, value_name = "NAME")]
        branch: String,
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum InvariantsVerb {
    /// One invariant: its registration, latest verdict, staleness, mutants, and a failing check's trace.
    Show { id: String },
    /// Run the checker (TLC for TLA+) on invariants of the System Spec and record the verdicts.
    Check {
        /// Invariant id in the spec document (`formal.tools.spec_doc`); without it, every formalized one.
        id: Option<String>,
        /// Every invariant with a formal block (the default without an id).
        #[arg(long, conflicts_with = "id")]
        all: bool,
        /// `bounded` (TLC, a finite instance) or `inductive` (needs Apalache); default: the claim's `mode:`.
        #[arg(long, value_parser = ["bounded", "inductive"])]
        mode: Option<String>,
        /// Explore behaviors of at most N steps (0 = the whole finite instance).
        #[arg(long, value_name = "N")]
        bound: Option<u32>,
    },
    /// The mutation gate: mutants of the spec's actions must be caught, or the invariant is Vacuous.
    Mutate { id: String },
}

#[derive(Subcommand, Debug)]
pub enum DoctorWhat {
    /// Validate a model provider route (`provider.<id>`): offline checks, then the API.
    Provider {
        /// Route name: openai, anthropic, ollama, scripted, or a `[provider.<name>]` table.
        id: String,
        /// offline = config, adapter, credential; catalog = + the model name; full = + one tiny request (spends tokens).
        #[arg(long, default_value = "offline")]
        tier: String,
        #[arg(long)]
        json: bool,
    },
    /// Local embeddings: which model is selected, whether its artifacts verify, and
    /// `--install` to download the pinned MiniLM once into `embed.local.model_dir`.
    Embed {
        #[arg(long)]
        install: bool,
    },
    /// The formal checkers behind spec mode: what is installed, and install TLC or Lean.
    Formal {
        /// `tla` (downloads tla2tools.jar) or `lean` (installs elan, the Lean toolchain manager).
        #[arg(long, value_name = "WHICH")]
        install: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum CapsVerb {
    /// The manifest card: kind, seams, handles, tools and classes, settings, entry kinds.
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ScheduleVerb {
    List {
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Cancel {
        id: String,
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum RalphVerb {
    /// Stop the running loop (or loop `--id`).
    Stop {
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum PlanModeVerb {
    /// Narrow a session to planning (read-only + asking); leaving goes through the agent's card or a preset change.
    On {
        #[arg(long)]
        reason: Option<String>,
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum SpecVerb {
    /// Leave spec mode: switch to this preset (default workspace).
    Exit { preset: Option<String> },
    /// Every claim in spec/SPEC.md with its verdict from the session log; extract freshness.
    Status {
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
    /// Regenerate §9/§10 of SPEC.md from the session log and re-extract spec/tla and spec/lean.
    Render {
        #[arg(long, value_name = "ID")]
        session: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum CredentialVerb {
    /// Store credential `<name>`; the value is read from the terminal (or one line of stdin).
    Set { name: String },
}

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// Provider id; omitted to choose interactively.
    pub provider: Option<String>,
    /// Show supported account-login providers without opening credentials or a browser.
    #[arg(long, conflicts_with_all = ["provider", "device_code", "gateway", "enterprise_domain"])]
    pub list: bool,
    /// Use device-code login instead of a localhost browser callback.
    #[arg(long)]
    pub device_code: bool,
    /// Print authorization URLs without launching the system browser.
    #[arg(long)]
    pub no_browser: bool,
    /// Radius gateway URL.
    #[arg(long)]
    pub gateway: Option<String>,
    /// GitHub Copilot Enterprise domain (default: github.com).
    #[arg(long)]
    pub enterprise_domain: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum AddWhat {
    /// An MCP server over stdio. Without --class it loads quarantined.
    Mcp {
        name: String,
        #[arg(long)]
        cmd: String,
        /// `VAR=value` or `VAR=cred:<name>`. Repeatable.
        #[arg(long = "env", value_name = "VAR=VALUE", value_parser = key_value)]
        env: Vec<(String, String)>,
        /// Consequence class of every tool it exposes (e.g. NetworkEgress).
        #[arg(long)]
        class: Option<String>,
    },
    /// A VM recipe over catalog media: what `vm_create` and `vm.recipe` build machines from.
    Recipe(RecipeArgs),
}
