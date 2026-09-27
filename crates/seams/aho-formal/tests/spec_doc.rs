#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design
use std::path::PathBuf;

use aho_formal::spec_doc::{EXTRACT_MARKER, GENERATED_NOTE};
use aho_formal::*;
use aho_log::{Entry, EntryKind, LogStore, MemoryLog};
use aho_types::{Hash, Principal};

const SPEC: &str = include_str!("fixtures/SPEC.md");

fn doc() -> SpecDoc {
    SpecDoc::parse(SPEC).unwrap()
}

fn log(kinds: Vec<EntryKind>) -> Vec<Entry> {
    let l = MemoryLog::new();
    for k in kinds {
        l.append(None, Principal::system("formal"), k).unwrap();
    }
    l.read_branch(l.head().unwrap()).unwrap()
}

#[test]
fn parses_sections_claims_blocks_and_actions() {
    let d = doc();
    assert_eq!(d.title, "Orders: System Spec");
    let order: Vec<SpecSection> = d.sections.iter().map(|s| s.which).collect();
    assert_eq!(order, SpecSection::ALL.to_vec());
    assert_eq!(d.actions, ["Charge", "Refund"]);
    let ids: Vec<&str> = d.claims.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["inv.order.no-double-charge", "inv.order.total", "inv.order.audited"]);
    let c = d.claim("inv.order.no-double-charge").unwrap();
    assert_eq!(c.statement(), "an order is charged at most once");
    assert_eq!(c.covers(), ["src/ledger.rs", "Charge"]);
    assert_eq!(c.mode(), Some(CheckMode::Bounded(4)));
    assert!(c.prose.contains("does not deduplicate"));
    let langs: Vec<BlockLang> = d.blocks.iter().map(|b| b.lang).collect();
    assert_eq!(langs, [BlockLang::Tla, BlockLang::Cfg, BlockLang::Lean]);
    assert!(d.blocks.iter().all(|b| b.section == Some(SpecSection::Invariants)));
    assert_eq!(d.tla_module_of("inv.order.no-double-charge").as_deref(), Some("Orders"));
    assert_eq!(d.formalized().into_iter().collect::<Vec<_>>(), ["inv.order.no-double-charge", "inv.order.total"]);
    assert!(d.formal_hash("inv.order.audited").is_none());
}

#[test]
fn section_headings_accept_the_usual_spellings_and_reject_mismatches() {
    assert_eq!(SpecSection::parse_heading("1 Purpose"), Some(SpecSection::Purpose));
    assert_eq!(SpecSection::parse_heading("6. Invariants"), Some(SpecSection::Invariants));
    assert_eq!(SpecSection::parse_heading("History"), Some(SpecSection::History));
    assert_eq!(SpecSection::parse_heading("9 Coverage (generated)"), Some(SpecSection::Coverage));
    assert_eq!(SpecSection::parse_heading("3 Purpose"), None, "number and title disagree");
    assert_eq!(SpecSection::parse_heading("Appendix"), None);
}

#[test]
fn malformed_documents_are_rejected_with_the_line() {
    let e = |t: &str| SpecDoc::parse(t).unwrap_err();
    assert!(matches!(e("## 1 Purpose\n## Appendix\n"), SpecError::UnknownSection { line: 2, .. }));
    assert!(matches!(e("## 2 Vocabulary\n## 1 Purpose\n"), SpecError::OutOfOrder { line: 2, .. }));
    assert!(matches!(e("## 1 Purpose\n## 1 Purpose\n"), SpecError::DuplicateSection { .. }));
    assert!(matches!(e("## 6 Invariants\n```tla\n---- MODULE A ----\n====\n```\n"), SpecError::BlockWithoutId { line: 2 }));
    assert!(matches!(e("## 6 Invariants\n```tla id=a\n---- MODULE A ----\n"), SpecError::UnterminatedFence { line: 2 }));
    assert!(matches!(e("## 6 Invariants\n### a\n- statement: x\n### a\n- statement: y\n"), SpecError::DuplicateClaim { line: 4, .. }));
    assert!(matches!(e("## 5 Actions\n- statement: misplaced\n"), SpecError::ClaimOutsideInvariants { line: 2, .. }));
    // headings inside fences are content, not structure
    let ok = SpecDoc::parse("## 1 Purpose\n```text\n## not a section\n```\n").unwrap();
    assert_eq!(ok.sections.len(), 1);
    // other languages are prose; they need no id
    assert!(SpecDoc::parse("## 3 Structure\n```rust\nfn f() {}\n```\n").unwrap().blocks.is_empty());
}

#[test]
fn extract_writes_marked_files_and_the_freshness_gate_compares_them() {
    let d = doc();
    let dir = tempfile::tempdir().unwrap();
    let spec = dir.path();
    let rep = extract(&d, spec).unwrap();
    let rel: Vec<PathBuf> = rep.written.iter().map(|p| p.strip_prefix(spec).unwrap().to_path_buf()).collect();
    assert_eq!(rel, [PathBuf::from("tla/Orders.tla"), PathBuf::from("tla/Orders.cfg"), PathBuf::from("lean/Spec/Order.lean")]);
    let tla = std::fs::read_to_string(spec.join("tla/Orders.tla")).unwrap();
    assert!(tla.lines().next().unwrap().contains(EXTRACT_MARKER));
    assert!(tla.contains("---- MODULE Orders ----") && tla.contains("NoDoubleCharge =="));
    let cfg = std::fs::read_to_string(spec.join("tla/Orders.cfg")).unwrap();
    assert!(cfg.starts_with("\\* ") && cfg.contains("INVARIANT NoDoubleCharge"));
    assert!(std::fs::read_to_string(spec.join("lean/Spec/Order.lean")).unwrap().contains("theorem total_nil"));
    assert!(freshness(&d, spec).unwrap().is_fresh());

    // a hand edit, a deleted extract, an orphaned extract, and a hand-written module
    std::fs::write(spec.join("tla/Orders.tla"), tla.replace("count + 1", "count + 2")).unwrap();
    std::fs::remove_file(spec.join("lean/Spec/Order.lean")).unwrap();
    std::fs::write(spec.join("tla/Old.tla"), format!("\\* {EXTRACT_MARKER} (abc); edit SPEC.md\n---- MODULE Old ----\n====\n")).unwrap();
    std::fs::write(spec.join("tla/Helpers.tla"), "---- MODULE Helpers ----\n====\n").unwrap();
    let f = freshness(&d, spec).unwrap();
    assert_eq!(f.stale, [spec.join("tla/Orders.tla")]);
    assert_eq!(f.missing, [spec.join("lean/Spec/Order.lean")]);
    assert_eq!(f.orphaned, [spec.join("tla/Old.tla")]);
    assert!(f.describe().contains("aho spec render"));

    let rep = extract(&d, spec).unwrap();
    assert_eq!(rep.removed, [spec.join("tla/Old.tla")]);
    assert!(freshness(&d, spec).unwrap().is_fresh());
    assert!(spec.join("tla/Helpers.tla").exists(), "files without the marker are never touched");

    // changing SPEC.md makes the old extracts stale
    let changed = SpecDoc::parse(&SPEC.replace("CONSTANT Orders = {o1, o2}", "CONSTANT Orders = {o1, o2, o3}")).unwrap();
    assert_eq!(freshness(&changed, spec).unwrap().stale, [spec.join("tla/Orders.cfg")]);
}

#[test]
fn extract_rejects_ambiguous_modules() {
    let two = "## 6 Invariants\n```tla id=a\n---- MODULE A ----\n====\n```\n```tla id=b\n---- MODULE A ----\n====\n```\n";
    assert!(matches!(SpecDoc::parse(two).unwrap().extracts(), Err(ExtractError::DuplicateModule { line: 6, first: 2, .. })));
    let headless = "## 6 Invariants\n```tla id=a\nVARIABLE x\n```\n";
    assert!(matches!(SpecDoc::parse(headless).unwrap().extracts(), Err(ExtractError::NoModuleHeader { line: 2 })));
    let cfg_only = "## 6 Invariants\n```cfg id=a\nINIT Init\n```\n";
    assert!(matches!(SpecDoc::parse(cfg_only).unwrap().extracts(), Err(ExtractError::CfgWithoutModule { .. })));
}

#[test]
fn coverage_lists_unformalized_claims_and_vacuous_invariants() {
    let d = doc();
    let h = d.formal_hash("inv.order.no-double-charge").unwrap();
    let entries = log(vec![
        EntryKind::InvariantRegistered {
            id: "inv.order.no-double-charge".into(),
            kind: FormalKind::Tla,
            statement: "an order is charged at most once".into(),
            covers: vec!["src/ledger.rs".into()],
        },
        EntryKind::InvariantChecked {
            id: "inv.order.no-double-charge".into(),
            mode: CheckMode::Bounded(4),
            verdict: Verdict::Pass,
            code_hash: h,
        },
        EntryKind::InvariantMutated { id: "inv.order.no-double-charge".into(), mutant: "drop-guard:Charge#1".into(), caught: false },
        EntryKind::InvariantMutated { id: "inv.order.no-double-charge".into(), mutant: "swap-cmp:Charge#1".into(), caught: false },
    ]);
    let ledger = Ledger::from_entries(&entries);
    assert_eq!(status(&d, &ledger, "inv.order.no-double-charge"), Status::Vacuous, "passed every mutant");
    assert_eq!(status(&d, &ledger, "inv.order.total"), Status::Draft, "never checked");
    assert_eq!(status(&d, &ledger, "inv.order.audited"), Status::Draft, "no formal block");

    let out = render_ledger(&d, &ledger);
    let cov = section(&out, "## 9 Coverage", "## 10 History");
    assert!(cov.contains(GENERATED_NOTE));
    let no_formal = block(&cov, "**Claims with no formal block**");
    assert!(no_formal.contains("`inv.order.audited`: every charge is visible in the audit log"), "{cov}");
    assert!(!no_formal.contains("inv.order.total"));
    let vacuous = block(&cov, "**Formal blocks that caught no mutant (Vacuous)**");
    assert!(vacuous.contains("`inv.order.no-double-charge`: ⚠ Vacuous, caught none of 2 mutants"), "{cov}");
    assert!(block(&cov, "**Formal blocks with no recorded mutant**").contains("`inv.order.total`"));
    assert!(cov.contains("| `inv.order.no-double-charge` | safety | tla | ⚠ Vacuous | 0/2 |"), "{cov}");
    assert!(block(&cov, "**Actions without an invariant**").contains("none."), "Charge is covered, Refund is named by actions:");
    assert!(!out.contains("hand-written text the renderer must replace"));

    // everything before §9 is kept byte for byte, and rendering is a fixed point
    let head = &SPEC[..SPEC.find("## 9 Coverage").unwrap()];
    assert!(out.starts_with(head.trim_end()));
    let again = render_ledger(&SpecDoc::parse(&out).unwrap(), &ledger);
    assert_eq!(again, out);

    // one caught mutant is enough for the invariant to count
    let mut more = entries.clone();
    more.extend(log(vec![EntryKind::InvariantMutated {
        id: "inv.order.no-double-charge".into(),
        mutant: "negate-guard:Charge#1".into(),
        caught: true,
    }]));
    assert_eq!(status(&d, &Ledger::from_entries(&more), "inv.order.no-double-charge"), Status::Pass);
}

#[test]
fn render_from_state_uses_the_folded_verdicts() {
    let d = doc();
    // verdicts are bound to the workspace tree they checked; the latest checkpoint is the current code
    let tree = Hash::of(b"the current tree");
    let entries = log(vec![
        EntryKind::InvariantChecked {
            id: "inv.order.no-double-charge".into(),
            mode: CheckMode::Bounded(4),
            verdict: Verdict::Vacuous,
            code_hash: tree,
        },
        EntryKind::InvariantChecked {
            id: "inv.order.total".into(),
            mode: CheckMode::Proof,
            verdict: Verdict::Pass,
            code_hash: Hash::of(b"an older tree"),
        },
        EntryKind::WorkspaceCheckpoint { id: aho_types::CheckpointId(1), tree, parent: None, cause: aho_log::CheckpointCause::Manual },
    ]);
    let state = aho_log::fold(&entries, &Default::default()).unwrap();
    let out = render_generated(&d, &state);
    let cov = section(&out, "## 9 Coverage", "## 10 History");
    assert!(block(&cov, "**Formal blocks that caught no mutant (Vacuous)**").contains("inv.order.no-double-charge"), "{cov}");
    assert!(block(&cov, "**Claims with no formal block**").contains("inv.order.audited"));
    assert!(cov.contains("| `inv.order.total` | functional | lean | ◌ Stale |"), "the code moved past its verdict: {cov}");
    assert!(!cov.contains("no recorded mutant"), "the fold does not carry mutants, so it does not claim there were none");
}

#[test]
fn history_shows_supersessions_goalpost_moves_and_withdrawals() {
    let text = SPEC.replace(
        "### inv.order.audited\n\n- statement: every charge is visible in the audit log",
        "### inv.order.audited\n\n- statement: every charge is visible in the audit log\n- supersedes: inv.order.logged — the old claim allowed a lost write",
    );
    let d = SpecDoc::parse(&text).unwrap();
    let entries = log(vec![
        EntryKind::InvariantRegistered {
            id: "inv.order.logged".into(),
            kind: FormalKind::Tla,
            statement: "charges are logged".into(),
            covers: vec![],
        },
        EntryKind::InvariantChecked {
            id: "inv.order.logged".into(),
            mode: CheckMode::Bounded(3),
            verdict: Verdict::Fail { counterexample: Hash::of(b"cx") },
            code_hash: Hash::ZERO,
        },
        EntryKind::InvariantRegistered {
            id: "inv.order.total".into(),
            kind: FormalKind::Lean,
            statement: "total is a sum".into(),
            covers: vec![],
        },
        EntryKind::InvariantRegistered {
            id: "inv.order.total".into(),
            kind: FormalKind::Lean,
            statement: "total is a sum of positive lines".into(),
            covers: vec![],
        },
        EntryKind::InvariantRegistered { id: "inv.gone".into(), kind: FormalKind::Tla, statement: "gone".into(), covers: vec![] },
    ]);
    let out = render_ledger(&d, &Ledger::from_entries(&entries));
    let hist = section(&out, "## 10 History", "\u{0}");
    assert!(hist.contains("`inv.order.logged` superseded by `inv.order.audited`: the old claim allowed a lost write. Old statement: “charges are logged”; last verdict: Fail."), "{hist}");
    assert!(hist.contains("`inv.order.total` goalpost moved"), "{hist}");
    assert!(hist.contains("“total is a sum” → “total is a sum of positive lines”"));
    assert!(hist.contains("`inv.gone` is in the log but no longer in §6"), "{hist}");
    assert!(!hist.contains("`inv.order.logged` is in the log but no longer"), "superseded ids are not withdrawals");
}

#[test]
fn missing_generated_sections_are_appended() {
    let short = SPEC[..SPEC.find("## 9 Coverage").unwrap()].to_string();
    let d = SpecDoc::parse(&short).unwrap();
    let out = render_ledger(&d, &Ledger::default());
    let re = SpecDoc::parse(&out).unwrap();
    assert_eq!(re.sections.len(), 10);
    assert!(out.contains("No invariant has been superseded or withdrawn."));
}

fn section(text: &str, from: &str, to: &str) -> String {
    let start = text.find(from).unwrap_or_else(|| panic!("no {from} in {text}"));
    let rest = &text[start..];
    let end = rest.find(to).unwrap_or(rest.len());
    rest[..end].to_string()
}

fn block(text: &str, title: &str) -> String {
    let start = text.find(title).unwrap_or_else(|| panic!("no {title} in {text}"));
    let rest = &text[start + title.len()..];
    let end = rest.find("\n**").unwrap_or(rest.len());
    rest[..end].to_string()
}
