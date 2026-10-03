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

/// The lab's fragment of built-in profile `coding`, layered after the spine's (`aho_base::CODING_PROFILE`).
pub const CODING_PROFILE: &str = include_str!("../profiles/coding.toml");

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
        .profile("coding", CODING_PROFILE)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use aho_compose::{Inputs, Planned, load};
    use aho_log::CapState;

    /// Tool names of the Active rows: what a session over this plan offers the model.
    fn visible(p: &Planned) -> BTreeSet<&'static str> {
        p.plan.active().filter_map(|row| p.manifests.iter().find(|m| m.id == row.id)).flat_map(|m| m.tools.iter().map(|d| d.name)).collect()
    }

    fn tools_of(p: &Planned, id: &str) -> Vec<&'static str> {
        p.manifests.iter().find(|m| m.id.name == id).map(|m| m.tools.iter().map(|d| d.name).collect()).unwrap_or_default()
    }

    const KEPT: [&str; 11] = [
        "tool-fs",
        "tool-bash",
        "tool-jobs",
        "tool-lsp",
        "tool-run-code",
        "tool-todo",
        "tool-subagent",
        "tool-skills",
        "tool-plan-mode",
        "tool-inspect",
        "mcp-stdio",
    ];
    const OFF: [&str; 10] = [
        "tool-goals",
        "tool-schedule",
        "tool-workflow",
        "tool-tasks",
        "tool-mcp-resources",
        "tool-session",
        "tool-memory",
        "memory-local",
        "tool-vm",
        "tool-display",
    ];

    /// A PATH holding executable stubs for what the kept capabilities probe for (the host world's
    /// `git`, a language server, a `run_code` runtime), so the plan does not depend on this machine.
    /// Planning only checks the executable bit; nothing runs them.
    fn stub_path(dir: &std::path::Path) -> String {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("stub-bin");
        std::fs::create_dir_all(&bin).unwrap();
        for name in ["git", "rust-analyzer", "python3"] {
            let p = bin.join(name);
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        bin.display().to_string()
    }

    #[test]
    fn coding_profile_offers_the_coding_tools_and_none_of_the_other_families() {
        let dir = tempfile::tempdir().unwrap();
        let mut i = Inputs::new(dir.path(), dir.path().join("home"));
        i.env.insert("PATH".into(), stub_path(dir.path()));
        // the profile names capabilities other slices may not have linked: still no diagnostic
        i.strict = true;
        let all = load(&super::lab(), &i).unwrap();
        i.profile = Some("coding".into());
        i.capture_files().unwrap();
        let coding = load(&super::lab(), &i).unwrap();
        let (before, after) = (visible(&all), visible(&coding));

        for id in KEPT {
            assert_eq!(coding.plan.row(id).map(|r| r.state.clone()), Some(CapState::Active), "{id} stays active");
        }
        for tool in ["read", "write", "bash", "job_wait", "aho_why", "unlock_request", "subagent", "run_code", "enter_plan_mode"] {
            assert!(after.contains(tool), "{tool} visible under --profile coding: {after:?}");
        }
        for id in OFF {
            let row = coding.plan.row(id).unwrap_or_else(|| panic!("{id} is linked in the lab"));
            assert!(matches!(&row.state, CapState::Disabled { why } if why.contains("Profile coding")), "{id}: {:?}", row.state);
            for tool in tools_of(&coding, id) {
                assert!(!after.contains(tool), "{tool} ({id}) hidden under --profile coding");
            }
        }
        // not vacuous: these families are on the model's list without the profile
        for id in ["tool-goals", "tool-schedule", "tool-workflow", "tool-tasks", "tool-mcp-resources", "tool-session", "tool-memory"] {
            let tools = tools_of(&all, id);
            assert!(!tools.is_empty() && tools.iter().all(|t| before.contains(t)), "{id} is visible without the profile");
        }
        // the profile only removes tools, except what it switches on
        let on: Vec<&str> = ["verify-gate", "context-files"].iter().flat_map(|id| tools_of(&coding, id)).collect();
        let added: Vec<_> = after.difference(&before).filter(|t| !on.contains(t)).collect();
        assert!(added.is_empty(), "{added:?}");
        for id in ["verify-gate", "context-files"] {
            if let Some(row) = coding.plan.row(id) {
                assert!(
                    !matches!(row.state, CapState::Disabled { .. } | CapState::Unlocked),
                    "{id} is on in the coding profile: {:?}",
                    row.state
                );
            }
        }
    }

    #[test]
    fn the_operators_coding_toml_layers_over_the_builtin_profile() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".aho/profiles")).unwrap();
        std::fs::write(home.join(".aho/profiles/coding.toml"), "[caps.tool-goals]\nenabled = true\n[caps.tool-lsp]\nenabled = false\n")
            .unwrap();
        let mut i = Inputs::new(dir.path(), &home);
        i.env.insert("PATH".into(), stub_path(dir.path()));
        i.profile = Some("coding".into());
        i.capture_files().unwrap();
        let p = load(&super::lab(), &i).unwrap();
        let state = |id: &str| p.plan.row(id).map(|r| r.state.clone());
        assert_eq!(state("tool-goals"), Some(CapState::Active), "the operator's file re-enables what the built-in turned off");
        assert!(matches!(state("tool-lsp"), Some(CapState::Disabled { .. })), "and turns off what it kept");
        assert!(matches!(state("tool-schedule"), Some(CapState::Disabled { .. })), "keys the file does not name stay built-in");
        assert!(visible(&p).contains("read"));

        // ~/.aho/config.toml outranks both profile layers
        std::fs::write(home.join(".aho/config.toml"), "[caps.tool-schedule]\nenabled = true\n").unwrap();
        i.capture_files().unwrap();
        let p = load(&super::lab(), &i).unwrap();
        assert_eq!(p.plan.row("tool-schedule").map(|r| r.state.clone()), Some(CapState::Active));
    }

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
