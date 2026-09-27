//! The agent reads the same projection the operator sees, and may only *request* changes.
//!
//! * `aho_why{id}`: `aho_inspect::why` for a capability id or a tool name (resolved to the
//!   capability that declares it, active or not): state, missing prerequisites with fixes,
//!   governing settings, classes, and the one command that would change it.
//! * `aho_inspect{view}`: `tree` (the `aho tree` screen) or `status` (preset, world, active caps).
//! * `unlock_request{id, reason}`: raises an approval card carrying the fix; never applies it.
//!
//! The session seeds two services this capability reads: `aho.plan` (`Arc<Plan>`) and
//! `aho.manifests` (`Arc<Vec<&'static Manifest>>`).

#![forbid(unsafe_code)]

use std::sync::Arc;

use aho_approval::ApprovalHandle;
use aho_cap::*;
use aho_log::ApprovalOutcome;
use aho_types::*;
use serde_json::json;

pub const PLAN_SERVICE: &str = "aho.plan";
pub const MANIFESTS_SERVICE: &str = "aho.manifests";

pub type Manifests = Arc<Vec<&'static Manifest>>;

fn why_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": { "id": { "type": "string", "description": "A capability id (e.g. `tool-vm`) or a tool name (e.g. `vm_start`)." } },
        "required": ["id"]
    }))
}

fn inspect_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": { "view": { "type": "string", "enum": ["tree", "status"] } },
        "required": ["view"]
    }))
}

fn unlock_schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "id": { "type": "string", "description": "Capability id or tool name to unlock." },
            "reason": { "type": "string", "description": "Why the task needs it; shown to the operator." }
        },
        "required": ["id", "reason"]
    }))
}

pub static TOOLS: &[ToolDecl] = &[
    ToolDecl::new(
        "aho_why",
        "Explain why a capability or tool is active, locked, disabled or unknown, and what would change it.",
        Consequence::ReadOnly,
    )
    .schema(why_schema)
    .safe()
    .max_inline(None),
    ToolDecl::new(
        "aho_inspect",
        "Show the capability tree or the session status, exactly as the operator sees them.",
        Consequence::ReadOnly,
    )
    .schema(inspect_schema)
    .safe(),
    ToolDecl::new(
        "unlock_request",
        "Ask the operator to enable or allow a capability. The operator decides; nothing changes unless they apply the fix.",
        Consequence::TalksToHuman,
    )
    .schema(unlock_schema),
];

pub struct InspectDeps {
    pub approval: ApprovalHandle,
    pub plan: Option<Arc<Plan>>,
    pub manifests: Option<Manifests>,
}

impl Deps for InspectDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Approval];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(InspectDeps { approval: r.handle()?, plan: r.service(PLAN_SERVICE), manifests: r.service(MANIFESTS_SERVICE) })
    }
}

#[derive(Clone)]
pub struct ToolInspect {
    approval: ApprovalHandle,
    plan: Arc<Plan>,
    manifests: Manifests,
}

impl ToolInspect {
    pub fn new(approval: ApprovalHandle, plan: Arc<Plan>, manifests: Manifests) -> Self {
        ToolInspect { approval, plan, manifests }
    }

    /// A tool name maps to the capability that declares it (aliases included).
    pub fn resolve_id<'a>(&'a self, id: &'a str) -> &'a str {
        if self.plan.row(id).is_some() {
            return id;
        }
        self.manifests.iter().find(|m| m.tools.iter().any(|t| t.name == id || t.aliases.contains(&id))).map(|m| m.id.name).unwrap_or(id)
    }

    pub fn why(&self, id: &str) -> ExecOutcome {
        let cap = self.resolve_id(id);
        match aho_inspect::why(&self.plan, cap) {
            Some(w) => {
                let value = serde_json::to_value(&w).unwrap_or(Value::Null);
                let text = serde_json::to_string_pretty(&value).unwrap_or_default();
                ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta: None }
            }
            None => ExecOutcome::err(
                "NOT_IN_COMPOSITION",
                format!(
                    "No capability or tool named `{id}` is linked into this composition. It cannot be enabled from here; an engineer would add its capability to the composition."
                ),
            ),
        }
    }

    pub fn inspect(&self, view: &str) -> ExecOutcome {
        match view {
            "tree" => {
                let nodes = aho_inspect::tree(&self.plan);
                ExecOutcome::text(aho_inspect::render_tree("aho", &nodes, &self.plan))
            }
            "status" => {
                let active: Vec<String> = self.plan.active().map(|r| r.id.name.to_string()).collect();
                let value = json!({
                    "preset": self.plan.preset,
                    "allowed": self.plan.allowed,
                    "world": self.plan.world.name,
                    "enforced_by": self.plan.world.enforced_by,
                    "active": active,
                    "plan": self.plan.hash.short(),
                });
                let text = serde_json::to_string_pretty(&value).unwrap_or_default();
                ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta: None }
            }
            other => ExecOutcome::err("BAD_VIEW", format!("`{other}` is not a view; use `tree` or `status`.")),
        }
    }

    pub async fn unlock(&self, id: &str, reason: &str) -> ExecOutcome {
        let cap = self.resolve_id(id).to_string();
        let Some(w) = aho_inspect::why(&self.plan, &cap) else {
            return self.why(id);
        };
        if matches!(w.state, aho_log::CapState::Active) {
            return ExecOutcome::text(format!("`{cap}` is already active; no unlock is needed."));
        }
        let fix = w.fix.clone().unwrap_or_else(|| format!("aho explain {cap}"));
        let card = aho_policy::ApprovalCard {
            tool: "unlock_request".into(),
            class: Consequence::TalksToHuman,
            args: json!({ "id": cap, "reason": reason, "fix": fix }),
            reason: aho_policy::DecisionReason::ToolCheck(format!("the agent asks to unlock `{cap}`")),
            world: self.plan.world.clone(),
            suggested_cut: Some(fix.clone()),
            rendered: format!("The agent asks you to unlock `{cap}`: {reason}\nTo do it, run: {fix}"),
        };
        let d = self.approval.0.request(card).await;
        let value = json!({ "id": cap, "fix": fix, "outcome": format!("{:?}", d.outcome) });
        let text = match d.outcome {
            ApprovalOutcome::AllowedOnce => format!(
                "The operator agreed to unlock `{cap}`. It takes effect once they run `{fix}` and the session reloads its plan; until then the capability stays as it is."
            ),
            other => aho_policy::render_outcome(other, "unlock_request"),
        };
        ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta: None }
    }
}

impl Capability for ToolInspect {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = InspectDeps;
    type Config = ();

    fn build(d: InspectDeps, _: ()) -> Result<Self, BuildError> {
        let plan = d.plan.ok_or(BuildError::Unsupported("the session did not seed `aho.plan`"))?;
        let manifests = d.manifests.unwrap_or_default();
        Ok(ToolInspect::new(d.approval, plan, manifests))
    }

    fn register(&self, r: &mut Registrar<'_>) {
        for decl in TOOLS {
            let me = self.clone();
            let exec: ToolExec = Arc::new(move |req: ExecRequest| {
                let me = me.clone();
                Box::pin(async move {
                    let a = &req.view.args;
                    let s = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                    match req.view.tool.as_str() {
                        "aho_why" => me.why(&s("id")),
                        "aho_inspect" => me.inspect(&s("view")),
                        "unlock_request" => me.unlock(&s("id"), &s("reason")).await,
                        other => ExecOutcome::err("UNKNOWN_TOOL", format!("tool-inspect has no tool `{other}`")),
                    }
                })
            });
            r.tool(decl, exec);
        }
    }
}

declare_capability!(
    ToolInspect,
    "tool-inspect",
    kind = Kind::Tool,
    provides = [],
    requires = [],
    tools = TOOLS,
    doc = "aho_why / aho_inspect / unlock_request: the agent reads the operator's projections and may only request changes."
);

#[cfg(test)]
mod tests {
    use super::*;
    use aho_approval::{Approval, AskPolicy, Decided};
    use aho_config::{ResolveInputs, Schema, resolve};

    struct Answer(ApprovalOutcome);
    impl Approval for Answer {
        fn policy(&self) -> AskPolicy {
            AskPolicy::Ask
        }
        fn request(&self, card: aho_policy::ApprovalCard) -> BoxFuture<'_, Decided> {
            assert!(card.rendered.contains("unlock `tool-vm`") || card.rendered.contains("unlock"));
            let o = self.0;
            Box::pin(async move { Decided { outcome: o, scope: aho_log::GrantScope::Once } })
        }
    }

    static VM_TOOLS: &[ToolDecl] = &[ToolDecl::new("vm_start", "start", Consequence::ControlsVM)];
    struct Vm;
    impl Capability for Vm {
        const ID: CapabilityId = Self::ID_;
        const MANIFEST: &'static Manifest = &Self::MANIFEST_;
        type Deps = VmDeps;
        type Config = ();
        fn build(_: VmDeps, _: ()) -> Result<Self, BuildError> {
            Ok(Vm)
        }
        fn register(&self, _: &mut Registrar<'_>) {}
    }
    struct VmDeps;
    impl Deps for VmDeps {
        const REQUIRES: &'static [HandleKind] = &[HandleKind::VmControl];
        fn resolve(_: &Resolver) -> Result<Self, MissingDep> {
            Ok(VmDeps)
        }
    }
    declare_capability!(
        Vm,
        "tool-vm",
        kind = Kind::Tool,
        provides = [],
        requires = [Requirement::Binary("qemu-system-aarch64")],
        tools = VM_TOOLS
    );

    fn tool(outcome: ApprovalOutcome) -> ToolInspect {
        let mut s = Schema::default();
        for k in core_keys() {
            s.add(k);
        }
        let l = resolve(&ResolveInputs::default(), &s, false).unwrap();
        let ms: Vec<&'static Manifest> = vec![Vm::MANIFEST, ToolInspect::MANIFEST];
        let plan = plan(&ms, &l, &Env::empty(Os::MacOs), &PolicyInput { preset: "workspace".into(), allowed: ConsequenceSet::ALL }, vec![])
            .unwrap();
        ToolInspect::new(ApprovalHandle(Arc::new(Answer(outcome))), Arc::new(plan), Arc::new(ms))
    }

    #[test]
    fn why_resolves_a_tool_name_to_its_capability_and_names_the_fix() {
        let t = tool(ApprovalOutcome::Rejected);
        let ExecOutcome::Done { value, .. } = t.why("vm_start") else { panic!() };
        assert_eq!(value["id"], "tool-vm");
        assert_eq!(value["fix"], "./install-system-deps.sh");
        assert!(matches!(t.why("nope"), ExecOutcome::Err { ref code, .. } if code == "NOT_IN_COMPOSITION"));
    }

    #[test]
    fn inspect_tree_is_the_operator_screen() {
        let t = tool(ApprovalOutcome::Rejected);
        let ExecOutcome::Done { content, .. } = t.inspect("tree") else { panic!() };
        let text = content[0].as_text().unwrap();
        assert!(text.contains("◌ tool-vm") && text.contains("Binary(qemu-system-aarch64)"), "{text}");
    }

    #[tokio::test]
    async fn unlock_request_only_asks_and_reports_the_answer() {
        let t = tool(ApprovalOutcome::AllowedOnce);
        let ExecOutcome::Done { value, content, .. } = t.unlock("vm_start", "the task needs a VM").await else { panic!() };
        assert_eq!(value["fix"], "./install-system-deps.sh");
        assert!(content[0].as_text().unwrap().contains("takes effect once they run"));
        let t = tool(ApprovalOutcome::Rejected);
        let ExecOutcome::Done { content, .. } = t.unlock("tool-vm", "x").await else { panic!() };
        assert!(content[0].as_text().unwrap().contains("rejected"), "{:?}", content);
    }
}
