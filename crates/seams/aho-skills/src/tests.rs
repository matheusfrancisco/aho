use super::*;

fn doc(s: &str) -> Untrusted<String> {
    Untrusted::new(s.to_string())
}

fn skill(name: &str, description: &str) -> Template {
    parse(doc(&format!("---\nname: {name}\ndescription: {description}\n---\nDo {name}.\n")), "x/SKILL.md").unwrap().0
}

#[test]
fn full_frontmatter_parses_every_field() {
    let text = "---\n\
name: pdf-extract\n\
description: >\n  Extract text\n  from PDFs\n\
when_to_use: the user attaches a PDF\n\
allowed-tools: Bash(pdftotext:*), Read Grep\n\
paths:\n  - \"docs/**/*.pdf\"\n  - '*.pdf'\n\
context: fork\n\
model: inherit\n\
argument-hint: <file>\n\
version: 1.2  # ignored\n\
metadata:\n  owner: docs-team\n\
---\n\n# PDF\nRun pdftotext on $ARGUMENTS.\n";
    let (t, dropped) = parse(doc(text), "skills/pdf-extract/SKILL.md").unwrap();
    assert!(dropped.is_empty());
    assert_eq!(t.name, "pdf-extract");
    assert_eq!(t.description, "Extract text from PDFs");
    assert_eq!(t.when_to_use.as_deref(), Some("the user attaches a PDF"));
    assert_eq!(t.allowed_tools, vec!["Bash(pdftotext:*)", "Read", "Grep"]);
    assert_eq!(t.paths, vec!["docs/**/*.pdf", "*.pdf"]);
    assert_eq!(t.context, Context::Fork);
    assert_eq!(t.model, None, "`inherit` means the session's model");
    assert_eq!(t.argument_hint.as_deref(), Some("<file>"));
    assert_eq!(t.invocable_by, Invocable::Both);
    assert_eq!(t.body, "# PDF\nRun pdftotext on $ARGUMENTS.");
}

#[test]
fn policy_shaped_keys_are_dropped_with_their_blocks_and_reported() {
    let text = "---\nname: deploy\nsandbox: full\nallow:\n  - bash(*)\n  - write(**)\nPolicy: {preset: full}\npermissions:\n  allow: [\"Bash\"]\nhooks:\n  PreToolUse: []\ndescription: ships it\n---\nsandbox: stays in the body\n";
    let (t, dropped) = parse(doc(text), "SKILL.md").unwrap();
    assert_eq!(dropped, vec!["sandbox", "allow", "Policy", "permissions", "hooks"]);
    assert_eq!(t.description, "ships it", "keys after a dropped block still parse");
    assert_eq!(t.body, "sandbox: stays in the body", "only frontmatter keys are dropped");
}

#[test]
fn legacy_camel_case_keys_are_rejected_with_the_spelling_to_use() {
    let text = "---\nname: a\ndescription: d\nallowedTools: [Read]\n---\n";
    let e = parse(doc(text), "s/SKILL.md").unwrap_err();
    assert_eq!(
        e,
        SkillError::LegacyKey { path: "s/SKILL.md".into(), line: 4, key: "allowedTools".into(), use_instead: "allowed-tools".into() }
    );
    assert!(e.to_string().contains("write `allowed-tools` instead"), "{e}");
    let e = parse(doc("---\nname: a\ndescription: d\nwhenToUse: x\n---\n"), "p").unwrap_err();
    assert!(matches!(e, SkillError::LegacyKey { ref use_instead, .. } if use_instead == "when-to-use"));
}

#[test]
fn name_is_required_and_kebab_case() {
    assert_eq!(parse(doc("---\ndescription: d\n---\n"), "p").unwrap_err(), SkillError::MissingKey { path: "p".into(), key: "name" });
    assert_eq!(parse(doc("---\nname: a\n---\n"), "p").unwrap_err(), SkillError::MissingKey { path: "p".into(), key: "description" });
    for bad in ["My-Skill", "my_skill", "-lead", "trail-", "dou--ble", "has space"] {
        let e = parse(doc(&format!("---\nname: \"{bad}\"\ndescription: d\n---\n")), "p").unwrap_err();
        assert!(matches!(e, SkillError::BadFrontmatter { line: 2, .. }), "{bad}: {e:?}");
    }
    let long = "a".repeat(MAX_NAME + 1);
    assert!(parse(doc(&format!("---\nname: {long}\ndescription: d\n---\n")), "p").is_err());
    assert!(parse(doc("---\nname: pdf-2-text\ndescription: d\n---\n"), "p").is_ok());
}

#[test]
fn malformed_frontmatter_reports_the_line() {
    assert_eq!(parse(doc("# just markdown\n"), "p").unwrap_err(), SkillError::NoFrontmatter { path: "p".into() });
    let e = parse(doc("---\nname: a\ndescription: d\ncontext: sideways\n---\n"), ".aho/skills/a/SKILL.md").unwrap_err();
    assert_eq!(
        e.to_string(),
        ".aho/skills/a/SKILL.md: frontmatter invalid at line 4: `context: sideways` must be inline or fork. The skill was not loaded; fix the file and reload."
    );
    let e = parse(doc("---\nname: a\ndescription: d\npaths: [\"src/[\"]\n---\n"), "p").unwrap_err();
    assert!(matches!(e, SkillError::BadFrontmatter { line: 4, .. }), "{e:?}");
    let e = parse(doc("---\nname: a\ndescription: d\nuser-invocable: false\ndisable-model-invocation: true\n---\n"), "p").unwrap_err();
    assert!(matches!(e, SkillError::BadFrontmatter { line: 4, .. }), "{e:?}");
}

#[test]
fn invocation_switches() {
    let op = parse(doc("---\nname: a\ndescription: d\ndisable-model-invocation: true\n---\n"), "p").unwrap().0;
    assert_eq!(op.invocable_by, Invocable::Operator);
    let ag = parse(doc("---\nname: a\ndescription: d\nuser-invocable: false\n---\n"), "p").unwrap().0;
    assert_eq!(ag.invocable_by, Invocable::Agent);
    let ex = parse(doc("---\nname: a\ndescription: d\ninvocable-by: operator\n---\n"), "p").unwrap().0;
    assert_eq!(ex.invocable_by, Invocable::Operator);
}

#[test]
fn catalog_digest_is_stable_and_moves_with_content() {
    let a = skill("alpha", "first");
    let b = skill("beta", "second <b>");
    let (t1, h1) = catalog_snapshot(&[a.clone(), b.clone()]);
    let (t2, h2) = catalog_snapshot(&[b.clone(), a.clone(), a.clone()]);
    assert_eq!((t1.as_str(), h1), (t2.as_str(), h2), "order and duplicates do not change the digest");
    assert_eq!(h1, Hash::of(t1.as_bytes()));
    assert!(
        t1.starts_with(
            "<available_skills>\n<skill name=\"alpha\">first</skill>\n<skill name=\"beta\">second &lt;b&gt;</skill>\n</available_skills>"
        ),
        "{t1}"
    );
    let mut b2 = b.clone();
    b2.description = "changed".into();
    assert_ne!(catalog_snapshot(&[a.clone(), b2]).1, h1);

    let mut op = skill("gamma", "operator only");
    op.invocable_by = Invocable::Operator;
    assert_eq!(catalog_snapshot(&[a.clone(), b.clone(), op]).1, h1, "operator-only skills stay out of the model's catalog");
    assert_eq!(catalog_snapshot(&[]).0, "");
}

#[test]
fn render_is_shared_and_substitutes_arguments() {
    let mut t = skill("deploy", "ships");
    t.body = "Deploy to $ARGUMENTS now. ($ARGUMENTS)".into();
    t.base_dir = "/ws/.aho/skills/deploy".into();
    t.resources = vec!["scripts/run.sh".into()];
    assert_eq!(
        render_invocation(&t, "  staging "),
        "<skill_content name=\"deploy\">\n<skill_resources>\nbase_dir: /ws/.aho/skills/deploy\n- scripts/run.sh\n</skill_resources>\n<skill_instructions>\nDeploy to staging now. (staging)\n</skill_instructions>\n</skill_content>"
    );
    assert_eq!(render_content(&t), render_invocation(&t, ""));
    t.body = "No placeholder.".into();
    assert!(render_invocation(&t, "x y").contains("No placeholder.\n\nARGUMENTS: x y\n</skill_instructions>"));
    assert!(!render_content(&t).contains("ARGUMENTS"));
}

#[test]
fn conditional_activation_by_touched_path() {
    let mut t = skill("rust-style", "style");
    assert!(!matches_paths(&t, "src/lib.rs"), "no paths: unconditional, never a path match");
    assert!(is_visible(&t, &[]));
    t.paths = vec!["src/**/*.rs".into(), "Cargo.toml".into()];
    assert!(matches_paths(&t, "src/a/b.rs"));
    assert!(matches_paths(&t, "./Cargo.toml"));
    assert!(!matches_paths(&t, "docs/x.md"));
    assert!(!is_visible(&t, &["docs/x.md".into()]));
    assert!(is_visible(&t, &["docs/x.md".into(), "src/main.rs".into()]));
    let (text, _) = catalog_snapshot_where(&[t.clone()], |x| is_visible(x, &[]));
    assert!(text.is_empty());
}
