//! The model-facing tools, started through a real Composition like a session starts them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use aho_cap::*;
use aho_config::{ResolveInputs, Schema, resolve};
use aho_log::State;
use aho_types::*;
use serde_json::json;
use tool_schedule::{NS, SERVICE, SchedulerLink, ToolSchedule, fold_schedules};

fn start(seed_link: bool) -> (Registry, Vec<(CapabilityId, String)>) {
    let comp = Composition::new("schedule-test").consume::<ToolSchedule>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let loadout = resolve(&ResolveInputs::default(), &schema, false).unwrap();
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
        seed.services.insert(SERVICE.into(), Arc::new(SchedulerLink::new()));
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

/// What the session does after the result: append `value.events`, fold, run the context hooks.
fn land(reg: &Registry, out: &ExecOutcome, state: &mut State) {
    let ExecOutcome::Done { value, .. } = out else { panic!("{out:?}") };
    for e in value["events"].as_array().unwrap() {
        assert_eq!(e["ns"], NS);
        let v = fold_schedules(state.custom.get(NS), e["name"].as_str().unwrap(), &e["body"]).unwrap();
        state.custom.insert(NS.into(), v);
    }
    for (_, f) in &reg.contexts {
        assert!(f(state).is_none());
    }
}

#[test]
fn without_a_hosted_scheduler_the_tools_do_not_start() {
    let (reg, broken) = start(false);
    assert!(reg.tool("schedule_create").is_none());
    assert_eq!(broken.len(), 1);
    assert!(broken[0].1.contains("no scheduler runs next to this session"), "{}", broken[0].1);
    let (reg, broken) = start(true);
    assert!(broken.is_empty() && reg.tool("schedule_create").is_some());
    assert!(ToolSchedule::MANIFEST.tools.iter().all(|t| t.class == Consequence::ReadOnly));
}

#[test]
fn create_list_cancel_round_trip_through_the_log() {
    let (reg, _) = start(true);
    let mut state = State::default();
    let out =
        call(&reg, "schedule_create", json!({"prompt": "check the build", "every": "30m", "label": "ci", "at": "", "cron": ""}), agent());
    assert!(text(&out).starts_with("Scheduled sched-1 \"ci\": every 30m; first fire "), "{}", text(&out));
    land(&reg, &out, &mut state);

    let out = call(&reg, "schedule_create", json!({"prompt": "standup", "cron": "0  9 * * 1-5"}), Principal::Operator);
    let ExecOutcome::Done { value, .. } = &out else { panic!() };
    assert_eq!(value["schedule"]["spec"], json!({"kind": "cron", "expr": "0 9 * * 1-5"}), "whitespace is normalized");
    land(&reg, &out, &mut state);

    let listed = text(&call(&reg, "schedule_list", json!({}), agent()));
    assert!(
        listed.starts_with("Now: ") && listed.contains("sched-1 \"ci\": every 30m") && listed.contains("sched-2: cron `0 9 * * 1-5` (UTC)"),
        "{listed}"
    );

    let refused = call(&reg, "schedule_cancel", json!({"id": "sched-2"}), agent());
    assert!(matches!(&refused, ExecOutcome::Err { code, .. } if code == "not_allowed"), "{}", text(&refused));
    let ok = call(&reg, "schedule_cancel", json!({"id": "sched-1"}), agent());
    assert_eq!(text(&ok), "Cancelled sched-1; it will not fire again.");
    land(&reg, &ok, &mut state);
    let listed = text(&call(&reg, "schedule_list", json!({}), agent()));
    assert!(listed.contains("Recently finished:\n- sched-1 \"ci\": every 30m; cancelled"), "{listed}");
}

#[test]
fn create_refuses_ambiguous_or_unreal_times_in_message_voice() {
    let (reg, _) = start(true);
    let refuse = |args: Value, code_want: &str, needle: &str| {
        let out = call(&reg, "schedule_create", args, agent());
        let ExecOutcome::Err { code, reason } = &out else { panic!("{out:?}") };
        assert_eq!(code, code_want, "{reason}");
        assert!(reason.contains(needle) && reason.contains("nothing was scheduled"), "{reason}");
    };
    refuse(json!({"prompt": "x"}), "bad_args", "pass exactly one of `at`, `every` or `cron`");
    refuse(json!({"prompt": "x", "every": "5m", "cron": "* * * * *"}), "bad_args", "more than one");
    refuse(json!({"prompt": "x", "cron": "61 * * * *"}), "bad_args", "minute must be 0–59");
    refuse(json!({"prompt": "x", "at": "2001-01-01T00:00:00Z"}), "never_fires", "has already passed");
    refuse(json!({"prompt": "x", "cron": "0 0 30 2 *"}), "never_fires", "never matches a real date");
    refuse(json!({"prompt": "x", "every": "10s"}), "too_frequent", "at most every 1m");
    refuse(json!({"prompt": "", "every": "10m"}), "bad_args", "the prompt is empty");
}
