//! The capability in a composition: Locked with a fix without its binaries, Active with them,
//! registers the `subagent` service and its lifecycle hooks, and refuses to build without the
//! parent log it mirrors into.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use aho_cap::*;
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_log::{CapState, LogStore, MemoryLog};
use aho_subagent::SubagentProvider;
use subagent_claude::{ClaudeCode, SubagentClaude};

fn comp() -> Composition {
    Composition::new("claude-test").provide::<SubagentClaude>()
}

fn plan_with(binaries: &[&str], toml: &str) -> (Plan, aho_config::Loadout) {
    let mut schema = Schema::default();
    for k in comp().config_keys() {
        schema.add(k);
    }
    let mut inputs = ResolveInputs::default();
    inputs.files.push(FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text: toml.into() });
    let loadout = resolve(&inputs, &schema, false).unwrap();
    let mut env = Env::empty(Os::current());
    env.binaries.extend(binaries.iter().map(|b| b.to_string()));
    let allowed = aho_policy::preset("dev").unwrap().cut(&BoundWorld::host());
    let plan = aho_cap::plan(&comp().manifests(), &loadout, &env, &PolicyInput { preset: "dev".into(), allowed }, vec![]).unwrap();
    (plan, loadout)
}

#[test]
fn without_tmux_or_claude_it_is_locked_with_a_fix() {
    let (plan, _) = plan_with(&["claude"], "");
    let row = plan.row("subagent-claude").unwrap();
    assert!(matches!(row.state, CapState::Locked { .. }), "{:?}", row.state);
    assert!(row.missing.iter().any(|m| m.requirement == "Binary(tmux)"), "{:?}", row.missing);
    assert_eq!(SubagentClaude::MANIFEST_.unlock.map(|f| f.cli), Some(subagent_claude::INSTALL));
}

#[test]
fn active_it_registers_the_subagent_service_and_hooks() {
    let (plan, loadout) = plan_with(&["tmux", "claude"], "[subagent.claude]\npoll_ms = 100\nblocked_after_ms = 9000\n");
    assert!(matches!(plan.row("subagent-claude").unwrap().state, CapState::Active));
    let mut seed = Registry::default();
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    seed.services.insert(foreign_cli::LOG_SERVICE.into(), Arc::new(log));
    let (reg, broken) = comp().start_seeded(&plan, &loadout, seed);
    assert!(broken.is_empty(), "{broken:?}");
    let p = reg.service::<Arc<dyn SubagentProvider>>(aho_subagent::SERVICE).unwrap();
    assert!(p.features().persona && !p.features().fork);
    assert!(p.children().is_empty());
    assert!(reg.service::<Arc<foreign_cli::ForeignProvider<ClaudeCode>>>(subagent_claude::PROVIDER).is_some());
    let events: Vec<HookEvent> = reg.hooks.iter().flat_map(|(_, h)| h.on.iter().copied()).collect();
    assert_eq!(events, vec![HookEvent::SessionStart, HookEvent::SessionEnd]);
    assert_eq!(SubagentClaude::MANIFEST_.entry_kinds, foreign_cli::mirror::ENTRY_KINDS);
}

#[test]
fn without_the_parent_log_it_does_not_build() {
    let (plan, loadout) = plan_with(&["tmux", "claude"], "");
    let (_, broken) = comp().start_seeded(&plan, &loadout, Registry::default());
    assert_eq!(broken.len(), 1);
    assert!(broken[0].1.contains("aho.log"), "{broken:?}");
}

#[test]
fn a_nonsense_setting_is_a_config_error() {
    let (plan, loadout) = plan_with(&["tmux", "claude"], "[subagent.claude]\npoll_ms = 0\n");
    let mut seed = Registry::default();
    let log: Arc<dyn LogStore> = Arc::new(MemoryLog::new());
    seed.services.insert(foreign_cli::LOG_SERVICE.into(), Arc::new(log));
    let (_, broken) = comp().start_seeded(&plan, &loadout, seed);
    assert!(broken.first().is_some_and(|(_, e)| e.contains("subagent.claude.poll_ms")), "{broken:?}");
}
