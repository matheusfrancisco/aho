//! Requirement + registration through the real planner and composition.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure

use aho_cap::{Composition, Env, Os, PolicyInput, Registry, plan};
use aho_config::{FileSnapshot, Layer, ResolveInputs, Schema, resolve};
use aho_llm::{LateResults, LlmRoute, Protocol};
use aho_log::CapState;
use aho_types::ConsequenceSet;
use llm_openai_completions::OpenAiCompletions;

struct Started {
    state: CapState,
    registry: Registry,
    broken: Vec<String>,
}

fn start(sets: &[(&str, &str)], file: Option<&str>) -> Started {
    let comp = Composition::new("t").provide::<OpenAiCompletions>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let inputs = ResolveInputs {
        sets: sets.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        files: file
            .map(|text| vec![FileSnapshot { path: "aho.toml".into(), layer: Layer::FileProject, text: text.into() }])
            .unwrap_or_default(),
        ..Default::default()
    };
    let loadout = resolve(&inputs, &schema, false).expect("resolve");
    let plan = plan(
        &comp.manifests(),
        &loadout,
        &Env::empty(Os::current()),
        &PolicyInput { preset: "workspace".into(), allowed: ConsequenceSet::ALL },
        vec![],
    )
    .expect("plan");
    let state = plan.row("llm-openai-completions").expect("row").state.clone();
    let (registry, broken) = comp.start(&plan, &loadout);
    Started { state, registry, broken: broken.into_iter().map(|(_, e)| e).collect() }
}

fn routes(r: &Registry) -> Vec<String> {
    r.services.keys().filter(|k| k.starts_with("llm:")).cloned().collect()
}

#[test]
fn nothing_configured_plans_locked() {
    let s = start(&[], None);
    assert!(matches!(s.state, CapState::Locked { .. }));
}

#[test]
fn ollama_only_setup_plans_active_and_registers_only_ollama() {
    let s = start(&[], Some("[provider.ollama]\nenabled = true\n"));
    assert_eq!(s.state, CapState::Active);
    assert!(s.broken.is_empty(), "{:?}", s.broken);
    assert_eq!(routes(&s.registry), ["llm:ollama"]);
    let route: LlmRoute = s.registry.service("llm:ollama").expect("route");
    let info = route.0.info();
    assert_eq!((info.protocol, info.late_results, info.route.as_str()), (Protocol::OpenAiCompletions, LateResults::ToolMessage, "ollama"));
}

#[test]
fn openai_key_and_custom_rows_each_get_a_route() {
    let file = "[provider.groq]\nprotocol = \"openai-completions\"\nbase_url = \"https://api.groq.com/openai/v1\"\nkey = \"cred:groq\"\n\n[credentials]\ngroq = \"gsk-1\"\n\n[provider.claude]\nprotocol = \"anthropic-messages\"\nbase_url = \"https://x\"\n";
    let s = start(&[("provider.openai.key", "sk-1")], Some(file));
    assert_eq!(s.state, CapState::Active);
    assert!(s.broken.is_empty(), "{:?}", s.broken);
    assert_eq!(routes(&s.registry), ["llm:groq", "llm:openai"], "rows of another protocol are not ours");
}

#[test]
fn custom_row_without_base_url_breaks_with_the_missing_key() {
    let s = start(&[("provider.openai.key", "sk-1"), ("provider.local.protocol", "openai-completions")], None);
    assert_eq!(s.state, CapState::Active);
    assert!(s.broken.iter().any(|e| e.contains("provider.local.base_url")), "{:?}", s.broken);
}

#[test]
fn subscription_routes_activate_without_an_openai_key() {
    for (key, service) in [("provider.openrouter.key", "llm:openrouter"), ("provider.github-copilot.key", "llm:github-copilot")] {
        let s = start(&[(key, "account-token")], None);
        assert_eq!(s.state, CapState::Active);
        assert!(s.broken.is_empty(), "{:?}", s.broken);
        assert_eq!(routes(&s.registry), [service]);
    }
}
