use std::path::Path;

use super::*;
use aho_config::{ResolveInputs, Schema, resolve};
use aho_policy::Untrusted;
use skills_fsdir::{SkillsConfig, SkillsFsdir};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn template(text: &str) -> Template {
    aho_skills::parse(Untrusted::new(text.to_string()), "t/SKILL.md").unwrap().0
}

/// A project with four skills, all enabled; `rust-style` is conditional, `release` operator-only.
fn start() -> (tempfile::TempDir, Registry) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(".aho/skills");
    write(
        &root.join("deploy/SKILL.md"),
        "---\nname: deploy\ndescription: Ship a build\nargument-hint: <env>\n---\nDeploy to $ARGUMENTS.\n",
    );
    write(&root.join("deploy/run.sh"), "#!/bin/sh\n");
    write(
        &root.join("rust-style.md"),
        "---\nname: rust-style\ndescription: Rust conventions\npaths: [\"src/**/*.rs\"]\n---\nUse rustfmt.\n",
    );
    write(
        &root.join("release.md"),
        "---\nname: release\ndescription: Cut a release\ndisable-model-invocation: true\n---\nTag $ARGUMENTS.\n",
    );
    write(&root.join("dormant.md"), "---\nname: dormant\ndescription: never enabled\n---\nx\n");

    let mut schema = Schema::default();
    core_keys().into_iter().chain(SkillsConfig::keys()).for_each(|k| schema.add(k));
    let mut sets: Vec<(String, String)> =
        ["deploy", "rust-style", "release"].iter().map(|n| (format!("skills.{n}.enabled"), "true".into())).collect();
    sets.push(("skills.project_dir".into(), dir.path().display().to_string()));
    sets.push(("skills.home_dir".into(), dir.path().join("nohome").display().to_string()));
    let l = resolve(&ResolveInputs { sets, ..ResolveInputs::default() }, &schema, false).unwrap();
    let comp = Composition::new("t").provide::<SkillsFsdir>().consume::<ToolSkills>();
    let policy = PolicyInput { preset: "read-only".into(), allowed: ConsequenceSet::of(&[Consequence::ReadOnly]) };
    let p = plan(&comp.manifests(), &l, &Env::empty(Os::current()), &policy, vec![]).unwrap();
    assert!(matches!(p.row("tool-skills").unwrap().state, aho_log::CapState::Active));
    let (reg, broken) = comp.start(&p, &l);
    assert!(broken.is_empty(), "{broken:?}");
    (dir, reg)
}

fn call(reg: &Registry, args: Value) -> ExecOutcome {
    let (decl, exec) = reg.tool("skill").unwrap();
    let view = ExecView {
        call: CallId("c1".into()),
        tool: decl.name.into(),
        class: decl.class,
        args,
        policy: PolicyView {
            epoch: Epoch(1),
            allowed: ConsequenceSet::ALL,
            world: BoundWorld::host(),
            workspace: "/ws".into(),
            home: "/h".into(),
        },
        by: Principal::Agent { id: AgentId(0), turn: TurnId(1) },
    };
    futures::executor::block_on(exec(ExecRequest { view, cancel: CancelToken::new(), progress: None }))
}

#[test]
fn skill_tool_renders_with_arguments_and_matches_the_slash_path() {
    let (dir, reg) = start();
    let ExecOutcome::Done { value, content, .. } = call(&reg, json!({"name": "deploy", "arguments": "staging"})) else { panic!() };
    let base = dir.path().join(".aho/skills/deploy").display().to_string();
    let expected = format!(
        "<skill_content name=\"deploy\">\n<skill_resources>\nbase_dir: {base}\n- run.sh\n</skill_resources>\n<skill_instructions>\nDeploy to staging.\n</skill_instructions>\n</skill_content>"
    );
    assert_eq!(content, vec![ContentBlock::text(expected.clone())]);
    assert_eq!(value["context"], "inline");
    assert_eq!(value["base_dir"], base.as_str());

    let templates = reg.service::<Arc<Vec<Template>>>(aho_skills::SERVICE).unwrap();
    assert_eq!(slash_invocation("/deploy staging", &templates).as_deref(), Some(expected.as_str()), "one render for both paths");
}

#[test]
fn skill_tool_refuses_unknown_inactive_and_operator_only_skills() {
    let (_dir, reg) = start();
    let ExecOutcome::Err { code, reason } = call(&reg, json!({"name": "dormant"})) else { panic!() };
    assert_eq!(code, "SKILL_NOT_FOUND");
    assert!(reason.contains("active skills: deploy, rust-style"), "unlocked skills are not loadable: {reason}");
    let ExecOutcome::Err { code, .. } = call(&reg, json!({"name": "release"})) else { panic!() };
    assert_eq!(code, "SKILL_OPERATOR_ONLY");
    let ExecOutcome::Err { code, .. } = call(&reg, json!({"arguments": "x"})) else { panic!() };
    assert_eq!(code, "BAD_ARGS");
}

#[test]
fn catalog_context_publishes_on_digest_change_and_admits_conditional_skills_on_path_match() {
    let (_dir, reg) = start();
    let (_, ctx) = &reg.contexts[0];
    let mut state = State::default();
    let first = ctx(&state).unwrap();
    assert!(first.contains("<skill name=\"deploy\">Ship a build Arguments: &lt;env&gt;</skill>"), "{first}");
    assert!(!first.contains("rust-style"), "conditional skill waits for a matching path");
    assert!(!first.contains("release"), "operator-only skills stay out of the model's catalog");
    assert_eq!(ctx(&state), None, "same digest → nothing new to log");

    state.calls.insert(CallId("r1".into()), ("read".into(), Consequence::ReadOnly, json!({"path": "docs/readme.md"})));
    assert_eq!(ctx(&state), None, "a non-matching path changes nothing");
    state.calls.insert(CallId("r2".into()), ("edit".into(), Consequence::WorkspaceWrite, json!({"file_path": "src/main.rs"})));
    let second = ctx(&state).unwrap();
    assert!(second.contains("<skill name=\"rust-style\">Rust conventions</skill>"), "{second}");
    assert_eq!(ctx(&state), None);
}

#[test]
fn slash_invocation_only_for_operator_invocable_names() {
    let both = template("---\nname: deploy\ndescription: d\n---\nGo $ARGUMENTS\n");
    let agent_only = template("---\nname: helper\ndescription: d\nuser-invocable: false\n---\nx\n");
    let ts = vec![both.clone(), agent_only];
    assert_eq!(slash_invocation("  /deploy  prod\nnow", &ts), Some(render_invocation(&both, "prod\nnow")));
    assert_eq!(slash_invocation("/deploy", &ts), Some(render_invocation(&both, "")));
    assert_eq!(slash_invocation("/helper", &ts), None, "agent-only skills are not slash commands");
    assert_eq!(slash_invocation("/deployx", &ts), None);
    assert_eq!(slash_invocation("please /deploy", &ts), None);
    assert_eq!(slash_invocation("/", &ts), None);
}
