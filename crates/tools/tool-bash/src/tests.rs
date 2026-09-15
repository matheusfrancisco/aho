use super::*;
use aho_config::{ResolveInputs, Schema, resolve};
use aho_proc::OpWait;
use std::path::Path;
use world_host::{HostSubprocess, HostWorld};

fn tool(root: &Path, ops: Option<OpTable>) -> ToolBash {
    let proc = SubprocessHandle(Arc::new(HostSubprocess::new(root)));
    ToolBash::build(BashDeps { proc, ops: ops.map(OperationsHandle) }, BashConfig::default()).unwrap()
}

fn view(tool: &'static str, call: &str, args: Value) -> ExecView {
    ExecView {
        call: CallId(call.into()),
        tool: tool.into(),
        class: Consequence::SpawnsProcess,
        args,
        policy: PolicyView {
            epoch: Epoch(0),
            allowed: ConsequenceSet::ALL,
            world: BoundWorld::host(),
            workspace: "/work/proj".into(),
            home: "/Users/someone".into(),
        },
        by: Principal::system("test"),
    }
}

fn req(tool: &'static str, call: &str, args: Value) -> ExecRequest {
    ExecRequest { view: view(tool, call, args), cancel: CancelToken::new(), progress: None }
}

fn text(o: &ExecOutcome) -> String {
    match o {
        ExecOutcome::Done { content, .. } => content.iter().filter_map(|b| b.as_text()).collect(),
        ExecOutcome::Err { code, reason } => format!("{code}: {reason}"),
        ExecOutcome::Cancelled => "<cancelled>".into(),
    }
}

async fn gone(pid: u32) -> bool {
    for _ in 0..200 {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(true);
        if !alive {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn bash_runs_end_to_end_streaming_into_progress_and_the_op() {
    let dir = tempfile::tempdir().unwrap();
    let ops = OpTable::new();
    let (op, _) = ops.register(CallId("c1".into()), "bash", OpKind::Process, false, true);
    let t = tool(dir.path(), Some(ops.clone()));
    let progress = Sender::default();
    let cmd = "echo one; echo 'AHO_PROGRESS {\"pct\":50}'; sleep 0.05; echo two >&2; sleep 0.05; printf tail; exit 3";
    let mut r = req("bash", "c1", json!({"command": cmd}));
    r.progress = Some(progress.clone());
    let marker = ops.wait(op, 5_000);
    let out = t.run("bash", &r).await;

    assert_eq!(text(&out), "one\nAHO_PROGRESS {\"pct\":50}\ntwo\ntail\n[exit 3]");
    let ExecOutcome::Done { value, meta, .. } = &out else { panic!("{out:?}") };
    assert_eq!((value["exit"].as_i64(), value["timed_out"].as_bool()), (Some(3), Some(false)));
    assert_eq!(
        meta,
        &Some(Card::Terminal { command: cmd.into(), output: "one\nAHO_PROGRESS {\"pct\":50}\ntwo\ntail".into(), exit: Some(3) })
    );
    assert_eq!(progress.drain(), vec!["one", "AHO_PROGRESS {\"pct\":50}", "two", "tail"]);
    assert_eq!(marker.await, Some(OpWait::Progress("{\"pct\":50}".into())), "the op saw the marker while the command ran");
    assert!(ops.output(op, 0).unwrap().0.starts_with("one\n"));
    assert!(ops.info(op).unwrap().identity.is_some(), "the child was attached to the op");

    let pwd = t.run("bash", &req("bash", "c2", json!({"command": "pwd"}))).await;
    assert_eq!(text(&pwd).trim(), std::fs::canonicalize(dir.path()).unwrap().to_string_lossy(), "runs in the workspace root");
    assert_eq!(text(&t.run("bash", &req("bash", "c3", json!({"command": "true"}))).await), "(no output)");
    assert!(text(&t.run("bash", &req("bash", "c4", json!({"command": "  "}))).await).starts_with("TOOL_BAD_ARGS"));
}

#[tokio::test]
async fn bash_timeout_kills_the_command_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let t = tool(dir.path(), None);
    let t0 = std::time::Instant::now();
    let out = t.run("bash", &req("bash", "c1", json!({"command": "echo started; sleep 30", "timeout_ms": 300}))).await;
    assert!(t0.elapsed() < Duration::from_secs(5));
    assert_eq!(text(&out), "started\n[killed after 300 ms: the time limit was reached; raise timeout_ms to allow longer]");
    let ExecOutcome::Done { value, .. } = &out else { panic!() };
    assert_eq!(value["timed_out"], json!(true));
}

#[tokio::test]
async fn cancelling_bash_kills_its_process_group_and_reports_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let t = tool(dir.path(), None);
    let progress = Sender::default();
    let mut r = req("bash", "c1", json!({"command": "sleep 30 & echo $!; wait"}));
    r.progress = Some(progress.clone());
    let cancel = r.cancel.clone();
    let p2 = progress.clone();
    let watcher = tokio::spawn(async move {
        loop {
            let seen = p2.drain().into_iter().next();
            if let Some(pid) = seen {
                cancel.cancel();
                return pid.trim().parse::<u32>().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    let t0 = std::time::Instant::now();
    let out = t.run("bash", &r).await;
    assert_eq!(out, ExecOutcome::Cancelled);
    assert!(t0.elapsed() < Duration::from_secs(5));
    let grandchild = watcher.await.unwrap();
    assert!(gone(grandchild).await, "backgrounded grandchild {grandchild} survived the cancel");
}

#[test]
fn check_denies_catastrophic_asks_confirm_and_honours_a_real_justification() {
    let v = view("bash", "c", json!({})).policy;
    match check_command(&json!({"command": "rm -rf /"}), &v) {
        Some(Decision::Deny(m)) => assert!(m.contains("the user must run this themselves"), "{m}"),
        other => panic!("{other:?}"),
    }
    let with_just = json!({"command": "rm -rf /", "justification": "the user explicitly asked me to wipe the root filesystem"});
    assert!(matches!(check_command(&with_just, &v), Some(Decision::Deny(_))), "Catastrophic is never lifted");

    match check_command(&json!({"command": "rm -rf /some/other"}), &v) {
        Some(Decision::Ask(m)) => assert!(m.contains("justification"), "{m}"),
        other => panic!("{other:?}"),
    }
    let bare = json!({"command": "rm -rf /some/other", "justification": "yes I am sure, please proceed ok"});
    assert!(matches!(check_command(&bare, &v), Some(Decision::Ask(_))), "a bare affirmation is not a justification");
    let real =
        json!({"command": "rm -rf /some/other", "justification": "user asked to delete the stale /some/other checkout from last week"});
    assert_eq!(check_command(&real, &v), None, "justified Confirm refines to passthrough; the chain still applies the ceiling");
    assert_eq!(check_command(&json!({"command": "ls -la"}), &v), None);
}

#[tokio::test]
async fn bash_persistent_keeps_cwd_and_env_and_survives_errors() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let t = tool(dir.path(), None);
    let run = |cmd: &str| {
        let t = t.clone();
        let r = req("bash_persistent", "p", json!({"command": cmd}));
        async move { t.run("bash_persistent", &r).await }
    };
    assert_eq!(text(&run("mkdir -p sub && cd sub && export AHO_X=42").await), "(no output)");
    assert_eq!(text(&run("pwd; echo $AHO_X").await), format!("{}/sub\n42\n", root.display()));
    let syntax = run("if then").await;
    assert!(text(&syntax).ends_with("[exit 2]"), "{}", text(&syntax));
    assert!(text(&syntax).contains("syntax error"), "{}", text(&syntax));
    assert_eq!(text(&run("echo $AHO_X").await), "42\n", "a syntax error does not end the shell");
    assert_eq!(text(&run("echo 'it'\"'\"'s'").await), "it's\n");
    assert_eq!(text(&run("cat").await), "(no output)", "commands cannot read the protocol from stdin");
    assert_eq!(text(&run("printf 'no newline'").await), "no newline\n");
    assert_eq!(text(&run("false").await), "(no output)\n[exit 1]");

    let ended = run("echo bye; exit 3").await;
    assert_eq!(text(&ended), "bye\n[exit 3]\n[the shell exited; the next call starts a fresh one (cwd and environment reset)]");
    assert_eq!(text(&run("pwd; echo \"[$AHO_X]\"").await), format!("{}\n[]\n", root.display()));
}

#[tokio::test]
async fn bash_persistent_timeout_kills_the_shell_and_the_next_call_starts_fresh() {
    let dir = tempfile::tempdir().unwrap();
    let t = tool(dir.path(), None);
    t.run("bash_persistent", &req("bash_persistent", "p1", json!({"command": "export AHO_Y=1"}))).await;
    let out = t.run("bash_persistent", &req("bash_persistent", "p2", json!({"command": "sleep 30", "timeout_ms": 300}))).await;
    assert!(text(&out).contains("[killed after 300 ms"), "{}", text(&out));
    let next = t.run("bash_persistent", &req("bash_persistent", "p3", json!({"command": "echo \"[$AHO_Y]\""}))).await;
    assert_eq!(text(&next), "[]\n");
}

#[test]
fn escalate_is_in_the_schema_only_when_the_world_confines() {
    let host = schema_bash(&BoundWorld::host()).0;
    assert!(host["properties"].get("escalate").is_none());
    let ns = BoundWorld { name: "namespace".into(), confines: true, enforced_by: "Linux namespaces".into() };
    assert!(schema_bash(&ns).0["properties"].get("escalate").is_some());
    assert_eq!(host["required"], json!(["command"]));
}

#[tokio::test]
async fn runs_through_a_composition_without_an_operations_table() {
    let ws = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let comp = Composition::new("t").provide::<HostWorld>().consume::<ToolBash>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let mut inputs = ResolveInputs::default();
    inputs.sets = vec![
        ("workspace.root".into(), ws.path().to_string_lossy().into_owned()),
        ("workspace.shadow_dir".into(), home.path().to_string_lossy().into_owned()),
    ];
    let l = resolve(&inputs, &schema, false).unwrap();
    let mut env = Env::empty(Os::current());
    env.binaries.insert("git".into());
    let policy = PolicyInput { preset: "dev".into(), allowed: ConsequenceSet::ALL };
    let p = plan(&comp.manifests(), &l, &env, &policy, vec![]).unwrap();
    let (reg, broken) = comp.start(&p, &l);
    assert!(broken.is_empty(), "{broken:?}");
    let (decl, exec) = reg.tool("bash").unwrap();
    assert_eq!((decl.op_kind, decl.rerunnable, decl.external_effect), (OpKind::Process, false, true));
    let out = exec(req("bash", "c1", json!({"command": "echo hi"}))).await;
    assert_eq!(text(&out), "hi\n");
}

// ───────────────────────────── pwsh parity ─────────────────────────────

fn pwsh_tool(root: &Path) -> ToolBash {
    let proc = SubprocessHandle(Arc::new(HostSubprocess::new(root)));
    ToolBash::build(BashDeps { proc, ops: None }, BashConfig { shell: ShellKind::Pwsh }).unwrap()
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

fn plan_with(shell: Option<&str>, pwsh_installed: bool) -> (aho_cap::Plan, aho_config::Loadout, Composition, tempfile::TempDir) {
    let ws = tempfile::tempdir().unwrap();
    let comp = Composition::new("t").provide::<HostWorld>().consume::<ToolBash>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let mut inputs = ResolveInputs::default();
    inputs.sets = vec![("workspace.root".into(), ws.path().to_string_lossy().into_owned())];
    if let Some(s) = shell {
        inputs.sets.push((SHELL_KEY.into(), s.into()));
    }
    let l = resolve(&inputs, &schema, false).unwrap();
    let mut env = Env::empty(Os::current());
    env.binaries.insert("git".into());
    if pwsh_installed {
        env.binaries.insert("pwsh".into());
    }
    let policy = PolicyInput { preset: "dev".into(), allowed: ConsequenceSet::ALL };
    (plan(&comp.manifests(), &l, &env, &policy, vec![]).unwrap(), l, comp, ws)
}

#[test]
fn the_shell_setting_parses_and_rejects_anything_else() {
    let (_, l, _, _ws) = plan_with(None, false);
    assert_eq!(BashConfig::from_loadout(&l).unwrap().shell, ShellKind::Posix);
    let (_, l, _, _ws) = plan_with(Some("pwsh"), false);
    assert_eq!(BashConfig::from_loadout(&l).unwrap().shell, ShellKind::Pwsh);
    let (_, l, _, _ws) = plan_with(Some("zsh"), false);
    assert!(matches!(BashConfig::from_loadout(&l), Err(ConfigError::Type { key, .. }) if key == SHELL_KEY));
}

#[test]
fn pwsh_without_the_binary_is_locked_with_a_fix_and_bash_stays_active() {
    let (p, _, _, _ws) = plan_with(None, false);
    assert!(matches!(p.row("tool-bash").unwrap().state, aho_log::CapState::Active));
    let (p, _, _, _ws) = plan_with(Some("pwsh"), false);
    let row = p.row("tool-bash").unwrap();
    assert!(matches!(&row.state, aho_log::CapState::Locked { missing } if missing[0].contains("Binary(pwsh)")), "{row:?}");
    assert_eq!(row.missing[0].fix.as_deref(), Some("aho set caps.tool-bash.shell bash"));
    let (p, l, comp, _ws) = plan_with(Some("pwsh"), true);
    assert!(matches!(p.row("tool-bash").unwrap().state, aho_log::CapState::Active));
    // Active under pwsh: the same tool names are registered with the PowerShell declarations.
    let (reg, broken) = comp.start(&p, &l);
    assert!(broken.is_empty(), "{broken:?}");
    for name in ["bash", "bash_persistent"] {
        let (decl, _) = reg.tool(name).unwrap();
        assert!(decl.description.contains("PowerShell") || decl.description.contains("pwsh"), "{}", decl.description);
        assert_eq!(decl.class, Consequence::SpawnsProcess);
    }
}

#[test]
fn under_pwsh_every_command_asks_and_catastrophic_ones_are_still_denied() {
    let v = view("bash", "c", json!({})).policy;
    assert!(
        matches!(check_pwsh(&json!({"command": "Get-ChildItem"}), &v), Some(Decision::Ask(m)) if m.contains("reads only POSIX shells"))
    );
    let justified =
        json!({"command": "Remove-Item -Recurse build", "justification": "the user asked to delete the build directory entirely"});
    assert!(matches!(check_pwsh(&justified, &v), Some(Decision::Ask(_))));
    assert!(matches!(check_pwsh(&json!({"command": "rm -rf ~"}), &v), Some(Decision::Deny(_))));
}

#[test]
fn the_pwsh_persistent_script_is_one_line_carrying_the_command_in_base64() {
    assert_eq!(pwsh::base64(b""), "");
    assert_eq!(pwsh::base64(b"f"), "Zg==");
    assert_eq!(pwsh::base64(b"fo"), "Zm8=");
    assert_eq!(pwsh::base64(b"foo"), "Zm9v");
    assert_eq!(pwsh::base64("Set-Location 'a b'\n$x = \"é\"".as_bytes()), "U2V0LUxvY2F0aW9uICdhIGInCiR4ID0gIsOpIg==");
    let s = pwsh::script("line one\nline 'two'", "__AHO_DONE_x_1");
    assert_eq!(s.matches('\n').count(), 1, "{s}");
    assert!(s.contains(&pwsh::base64(b"line one\nline 'two'")) && s.contains("`n__AHO_DONE_x_1 $__aho_rc`n"), "{s}");
}

#[tokio::test]
async fn pwsh_runs_commands_when_installed() {
    if !on_path("pwsh") {
        println!("skipped: `pwsh` is not on PATH on this host; the pwsh execution paths were not exercised");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let t = pwsh_tool(dir.path());
    let out = t.run("bash", &req("bash", "p1", json!({"command": "Write-Output (6 * 7); exit 3"}))).await;
    assert!(text(&out).starts_with("42\n") && text(&out).contains("[exit 3]"), "{}", text(&out));
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let a = t.run("bash_persistent", &req("bash_persistent", "p2", json!({"command": "Set-Location sub; $x = 'kept'"}))).await;
    assert_eq!(text(&a), "(no output)");
    let b = t
        .run("bash_persistent", &req("bash_persistent", "p3", json!({"command": "Write-Output $x (Split-Path -Leaf (Get-Location))"})))
        .await;
    assert_eq!(text(&b), "kept\nsub\n");
    let c = t.run("bash_persistent", &req("bash_persistent", "p4", json!({"command": "this-is-not-a-command"}))).await;
    assert!(text(&c).contains("[exit 1]"), "{}", text(&c));
    let d = t.run("bash_persistent", &req("bash_persistent", "p5", json!({"command": "Write-Output $x"}))).await;
    assert_eq!(text(&d), "kept\n", "a failing command must not end the pwsh process");
}
