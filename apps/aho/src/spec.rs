//! Spec mode from the CLI. Entering and leaving is a preset switch (plan → diff → apply);
//! `status` and `render` read `spec/SPEC.md` against the session log's formal entries.

use std::path::{Path, PathBuf};

use aho_formal::{Ledger, SpecDoc};

use crate::cli::SpecVerb;
use crate::ctx::{Ctx, Fail, branch, err, find_session};

const SPEC_PRESET: &str = "spec";
const DEFAULT_DOC: &str = "spec/SPEC.md";

pub async fn run(ctx: &Ctx, sub: Option<&SpecVerb>) -> Result<(), Fail> {
    match sub {
        None => {
            println!("spec mode: the agent reads the code, writes only under spec/, and runs the checkers;");
            println!("every invariant it proposes needs your approval. Leave with `aho spec exit`.");
            crate::config::preset(ctx, SPEC_PRESET).await
        }
        Some(SpecVerb::Exit { preset }) => {
            let to = preset.as_deref().unwrap_or(aho_compose::DEFAULT_PRESET);
            if to == SPEC_PRESET {
                return Err(err("`spec exit` leaves spec mode; name the preset to return to (default workspace)"));
            }
            crate::config::preset(ctx, to).await
        }
        Some(SpecVerb::Status { session }) => status(ctx, session.as_deref()),
        Some(SpecVerb::Render { session }) => render(ctx, session.as_deref()),
    }
}

fn doc_path(ctx: &Ctx, p: &aho_compose::Planned) -> PathBuf {
    ctx.project.join(p.loadout.str("formal.tools.spec_doc").unwrap_or(DEFAULT_DOC))
}

fn read_doc(path: &Path) -> Result<(String, SpecDoc), Fail> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        err(format!("{} cannot be read ({e}); start one in spec mode (`aho spec`) or set formal.tools.spec_doc", path.display()))
    })?;
    let doc = SpecDoc::parse(&text).map_err(|e| err(format!("{} does not parse: {e}", path.display())))?;
    Ok((text, doc))
}

fn ledger(ctx: &Ctx, p: &aho_compose::Planned, session: Option<&str>, required: bool) -> Result<(Ledger, Option<String>), Fail> {
    match find_session(&ctx.session_dir(p), session) {
        Ok(s) => {
            let (_, entries) = branch(&s.path)?;
            Ok((Ledger::from_entries(&entries), Some(crate::ctx::hex(s.id))))
        }
        Err(e) if required || session.is_some() => Err(e),
        Err(_) => Ok((Ledger::default(), None)),
    }
}

fn status(ctx: &Ctx, session: Option<&str>) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let path = doc_path(ctx, &p);
    let (_, doc) = read_doc(&path)?;
    let (ledger, sid) = ledger(ctx, &p, session, false)?;
    println!("{}  (verdicts from {})", path.display(), sid.map(|s| format!("session {s}")).unwrap_or_else(|| "no session yet".into()));
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for c in &doc.claims {
        let st = aho_formal::status(&doc, &ledger, &c.id);
        *counts.entry(format!("{st:?}")).or_default() += 1;
        println!("  {:<11} {:<32} {}", st.glyph(), c.id, surface_print::clip(c.statement(), 60));
    }
    let summary: Vec<String> = counts.iter().map(|(k, n)| format!("{n} {k}")).collect();
    println!("invariants  {} registered · {}", doc.claims.len(), if summary.is_empty() { "none".into() } else { summary.join(" · ") });
    let covered: std::collections::BTreeSet<String> = doc.claims.iter().flat_map(|c| c.actions()).collect();
    let bare: Vec<&String> = doc.actions.iter().filter(|a| !covered.contains(*a)).collect();
    if !bare.is_empty() {
        println!(
            "coverage    actions without invariant: {} ({})",
            bare.len(),
            bare.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
    let dir = path.parent().unwrap_or(&ctx.project);
    match aho_formal::freshness(&doc, dir) {
        Ok(f) if f.is_fresh() => println!("extracts    fresh"),
        Ok(f) => println!("extracts    {}", f.describe()),
        Err(e) => println!("extracts    cannot be checked: {e}"),
    }
    Ok(())
}

fn render(ctx: &Ctx, session: Option<&str>) -> Result<(), Fail> {
    let p = ctx.load(&ctx.inputs(&[], false)?)?;
    let path = doc_path(ctx, &p);
    let (before, doc) = read_doc(&path)?;
    let (ledger, sid) = ledger(ctx, &p, session, true)?;
    let out = aho_formal::render_ledger(&doc, &ledger);
    if out != before {
        std::fs::write(&path, &out).map_err(|e| err(format!("could not write {} ({e})", path.display())))?;
        println!("wrote {}  (§9 Coverage and §10 History from session {})", path.display(), sid.unwrap_or_default());
    } else {
        println!("{} is already current", path.display());
    }
    let rendered = SpecDoc::parse(&out).map_err(|e| err(format!("the rendered document does not parse: {e}")))?;
    let report = aho_formal::extract(&rendered, path.parent().unwrap_or(&ctx.project)).map_err(|e| err(e.to_string()))?;
    for w in &report.written {
        println!("extracted {}", w.display());
    }
    for r in &report.removed {
        println!("removed   {}", r.display());
    }
    Ok(())
}
