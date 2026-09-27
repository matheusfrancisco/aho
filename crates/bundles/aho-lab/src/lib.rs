//! The web product plus the formal checkers behind spec mode (TLC for TLA+, Lean), long-term
//! memory over local embeddings, the session query tools, Claude Code and Codex as subagent
//! providers (observed, never wrapped; `aho set seams.subagent subagent-claude`), self-dev reload
//! (`aho selfdev reload`), and the VM lab: `vm-qemu` under an operator lease,
//! the 13 `vm_*` tools, `world = vm` (the session's sandbox VM), and the session desktop tools.
//! Checkers, foreign CLIs and QEMU lock with a named fix when their binaries are missing; nothing
//! here widens the preset (ControlsVM / ControlsDesktop still need `aho preset lab` or an approval).
#![cfg_attr(test, allow(clippy::disallowed_methods))] // unit-test fixtures only
#![forbid(unsafe_code)]

use aho_cap::Composition;

pub const NAME: &str = "aho-lab";

pub fn lab() -> Composition {
    Composition::new(NAME)
        .include(aho_web::web())
        .provide::<formal_tlc::FormalTlc>()
        .provide::<formal_lean::FormalLean>()
        .consume::<tool_formal::ToolFormal>()
        .provide::<embed_local::EmbedLocal>()
        .provide::<memory_local::MemoryLocal>()
        .consume::<tool_memory::ToolMemory>()
        // Session query tools are opt-in (dsh left them off shipped rosters); the lab ships them.
        .consume::<tool_session::ToolSession>()
        .provide::<subagent_claude::SubagentClaude>()
        .provide::<subagent_codex::SubagentCodex>()
        .provide::<selfdev::Selfdev>()
        .provide::<world_vm::VmQemu>()
        .provide::<world_vm::VmWorld>()
        .consume::<tool_vm::ToolVm>()
        .consume::<tool_display::ToolDisplay>()
}

#[cfg(test)]
mod tests {
    use aho_compose::{Inputs, load};
    use aho_log::CapState;

    #[test]
    fn checkers_without_binaries_lock_with_a_fix_and_memory_binds_its_embedder() {
        let dir = tempfile::tempdir().unwrap();
        let mut i = Inputs::new(dir.path(), dir.path().join("home"));
        i.env.insert("PATH".into(), dir.path().join("empty-path").display().to_string());
        let p = load(&super::lab(), &i).unwrap();
        let tlc = p.plan.row("formal-tlc").unwrap();
        assert!(matches!(tlc.state, CapState::Locked { .. }), "no java on PATH");
        assert!(tlc.missing.iter().all(|m| m.fix.is_some()), "every locked prerequisite names a fix: {:?}", tlc.missing);
        assert_eq!(p.plan.row("memory-local").map(|r| r.state.clone()), Some(CapState::Active), "embed-local satisfies Seam(embed)");
        assert_eq!(p.plan.preset, "workspace", "the lab product does not widen the shipped preset");
        let selfdev = p.plan.row("selfdev").unwrap();
        assert!(matches!(selfdev.state, CapState::Locked { .. }), "self-dev needs an active host world (no git here): {:?}", selfdev.state);
        assert!(selfdev.missing.iter().any(|m| m.requirement.contains("world-host")), "{:?}", selfdev.missing);
    }

    #[test]
    fn foreign_cli_subagents_wait_for_the_seam_and_then_name_their_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let mut i = Inputs::new(dir.path(), dir.path().join("home"));
        i.env.insert("PATH".into(), dir.path().join("empty-path").display().to_string());
        let p = load(&super::lab(), &i).unwrap();
        for id in ["subagent-claude", "subagent-codex"] {
            assert!(matches!(p.plan.row(id).unwrap().state, CapState::Unlocked), "{id}: in-process subagents are bound by default");
        }
        i.sets = vec![("seams.subagent".into(), "subagent-claude".into())];
        let p = load(&super::lab(), &i).unwrap();
        let claude = p.plan.row("subagent-claude").unwrap();
        assert!(matches!(claude.state, CapState::Locked { .. }), "no tmux or claude on PATH: {:?}", claude.state);
        assert!(claude.missing.iter().all(|m| m.fix.is_some()), "{:?}", claude.missing);
    }
}
