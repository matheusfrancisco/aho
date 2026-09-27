//! The model-facing ralph tools, started through a real Composition.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use aho_cap::*;
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_log::State;
use aho_types::*;
use serde_json::json;
use tool_workflow::{LoopLink, NS, SERVICE, ToolWorkflow, fold_workflows};

fn start(seed_link: bool, toml: &str) -> (Registry, Vec<(CapabilityId, String)>) {
    let comp = Composition::new("workflow-test").consume::<ToolWorkflow>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let mut inputs = ResolveInputs::default();
    inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text: toml.into() });
    let loadout = resolve(&inputs, &schema, false).unwrap();
    let p = plan(
        &comp.manifests(),
        &loadout,
        &Env::empty(Os::current()),
        &PolicyInput { preset: "workspace".into(), allowed: ConsequenceSet::ALL },
        vec![],
    )
    .unwrap();
    let mut seed = Registry::default();
    if seed_link {
        seed.services.insert(SERVICE.into(), Arc::new(LoopLink::new()));
    }
    comp.start_seeded(&p, &loadout, seed)
}

fn call(reg: &Registry, tool: &str, args: Value, by: Principal) -> ExecOutcome {
    let (decl, exec) = reg.tool(tool).unwrap();
    let view = ExecView {
        call: CallId("c1".into()),
        tool: tool.into(),
        class: decl.class,
        args,
        policy: PolicyView {
            epoch: Epoch(1),
            allowed: ConsequenceSet::ALL,
            world: BoundWorld::host(),
            workspace: "/ws".into(),
            home: "/home".into(),
        },
        by,
    };
    futures::executor::block_on(exec(ExecRequest { view, cancel: CancelToken::new(), progress: None }))
}

fn agent() -> Principal {
    Principal::Agent { id: AgentId(0), turn: TurnId(1) }
}

fn text(o: &ExecOutcome) -> String {
    match o {
        ExecOutcome::Done { content, .. } => content[0].as_text().unwrap().to_string(),
        ExecOutcome::Err { code, reason } => format!("{code}: {reason}"),
        ExecOutcome::Cancelled => "cancelled".into(),
    }
}

fn land(reg: &Registry, out: &ExecOutcome, state: &mut State) {
    let ExecOutcome::Done { value, .. } = out else { panic!("{out:?}") };
    for e in value["events"].as_array().unwrap() {
        assert_eq!(e["ns"], NS);
        let v = fold_workflows(state.custom.get(NS), e["name"].as_str().unwrap(), &e["body"]).unwrap();
        state.custom.insert(NS.into(), v);
    }
    for (_, f) in &reg.contexts {
        assert!(f(state).is_none());
    }
}

#[test]
fn without_a_hosted_loop_driver_the_tools_do_not_start() {
    let (reg, broken) = start(false, "");
    assert!(reg.tool("ralph_start").is_none());
    assert!(broken[0].1.contains("no loop driver runs next to this session"), "{}", broken[0].1);
    assert!(ToolWorkflow::MANIFEST.tools.iter().all(|t| t.class == Consequence::ReadOnly));
}

#[test]
fn start_status_stop_through_the_log() {
    let (reg, broken) = start(true, "[workflows]\nmax_iterations = 8\ndefault_iterations = 4\n");
    assert!(broken.is_empty(), "{broken:?}");
    let mut state = State::default();
    assert_eq!(text(&call(&reg, "ralph_status", json!({}), agent())), "No ralph loop has run in this session.");

    let out = call(&reg, "ralph_start", json!({"prompt": "port the parser", "completion_promise": "PARSER PORTED"}), agent());
    assert!(
        text(&out).starts_with("Started ralph-1. After this turn ends, the session gets the prompt again, up to 4 times"),
        "{}",
        text(&out)
    );
    assert!(text(&out).contains("<promise>PARSER PORTED</promise>"));
    land(&reg, &out, &mut state);

    let again = call(&reg, "ralph_start", json!({"prompt": "x", "completion_promise": "Y"}), agent());
    assert!(matches!(&again, ExecOutcome::Err { code, .. } if code == "another_running"), "{}", text(&again));
    let status = text(&call(&reg, "ralph_status", json!({}), agent()));
    assert!(status.starts_with("ralph-1 is running after 0 of 4 iterations"), "{status}");

    let out = call(&reg, "ralph_stop", json!({"reason": "blocked on review"}), agent());
    assert_eq!(text(&out), "Stopped ralph-1; the prompt will not be given again.");
    land(&reg, &out, &mut state);
    let status = text(&call(&reg, "ralph_status", json!({"id": "ralph-1"}), agent()));
    assert!(status.contains("is stopped") && status.contains("Reason: blocked on review"), "{status}");
}

#[test]
fn start_refuses_budgets_past_the_operator_cap() {
    let (reg, _) = start(true, "[workflows]\nmax_iterations = 8\n");
    let out = call(&reg, "ralph_start", json!({"prompt": "x", "completion_promise": "Y", "max_iterations": 9}), agent());
    let ExecOutcome::Err { code, reason } = &out else { panic!() };
    assert_eq!(code, "bad_args");
    assert!(reason.contains("more than the 8") && reason.contains("workflows.max_iterations"), "{reason}");
    let out = call(&reg, "ralph_start", json!({"prompt": "x", "completion_promise": ""}), agent());
    assert!(text(&out).contains("`completion_promise` is empty"), "{}", text(&out));
}
