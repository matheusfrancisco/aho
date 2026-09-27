//! Everything TLC reads that aho writes: the `.cfg` for a `CheckConfig`, the depth-bound
//! wrapper module for `Bounded(n)`, and the trace module for trace validation.

use aho_formal::{CheckConfig, CheckMode, FormalError, tla};
use aho_types::Value;

/// Name of the wrapper module `Bounded(n)` checks run (it `EXTENDS` the spec and `TLC`).
pub const BOUND_MODULE: &str = "AhoBounded";
pub const BOUND_OP: &str = "AhoDepthBound";
/// Name of the generated trace module.
pub const TRACE_MODULE: &str = "AhoTrace";
pub const TRACE_INDEX: &str = "ahoTraceIdx";

/// `CheckConfig` → TLC `.cfg` text. `extra_constraints` are appended to the configured ones
/// (the depth bound). Names must be identifiers and values single-line; anything else would
/// change the meaning of the file, so it is refused.
pub fn render_cfg(cfg: &CheckConfig, extra_constraints: &[&str]) -> Result<String, FormalError> {
    let ident = |kind: &str, n: &str| {
        if tla::is_ident(n) {
            Ok(())
        } else {
            Err(FormalError::Invalid(format!("{kind} `{n}` is not a TLA+ identifier; nothing was checked")))
        }
    };
    let mut out = String::new();
    if !cfg.constants.is_empty() {
        out.push_str("CONSTANTS\n");
        for (k, v) in &cfg.constants {
            ident("constant", k)?;
            if v.contains('\n') || v.trim().is_empty() {
                return Err(FormalError::Invalid(format!("constant `{k}` needs a one-line TLA+ value, got {v:?}")));
            }
            match v.trim().strip_prefix("<-") {
                Some(op) => out.push_str(&format!("    {k} <- {}\n", op.trim())),
                None => out.push_str(&format!("    {k} = {}\n", v.trim())),
            }
        }
    }
    ident("INIT", &cfg.init)?;
    ident("NEXT", &cfg.next)?;
    out.push_str(&format!("INIT {}\nNEXT {}\n", cfg.init, cfg.next));
    for c in cfg.constraints.iter().map(String::as_str).chain(extra_constraints.iter().copied()) {
        ident("constraint", c)?;
        out.push_str(&format!("CONSTRAINT {c}\n"));
    }
    for i in &cfg.invariants {
        ident("invariant", i)?;
        out.push_str(&format!("INVARIANT {i}\n"));
    }
    for p in &cfg.properties {
        ident("property", p)?;
        out.push_str(&format!("PROPERTY {p}\n"));
    }
    out.push_str(if cfg.check_deadlock { "CHECK_DEADLOCK TRUE\n" } else { "CHECK_DEADLOCK FALSE\n" });
    Ok(out)
}

/// The module a `Bounded(n)` check runs: every behavior of at most `n` steps (TLC's level of an
/// initial state is 1).
pub fn bound_module(spec_module: &str, n: u32) -> String {
    format!("---- MODULE {BOUND_MODULE} ----\nEXTENDS {spec_module}, TLC\n{BOUND_OP} == TLCGet(\"level\") <= {}\n====\n", u64::from(n) + 1)
}

pub fn depth_bound(mode: CheckMode) -> Option<u32> {
    match mode {
        CheckMode::Bounded(n) if n > 0 => Some(n),
        _ => None,
    }
}

/// `CONSTANT`/`CONSTANTS` sections of a TLC cfg, verbatim (for the trace cfg).
pub fn constant_sections(cfg_text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for l in cfg_text.lines() {
        let first = l.split_whitespace().next().unwrap_or("");
        if tla::CFG_KEYWORDS.contains(&first) {
            inside = first == "CONSTANT" || first == "CONSTANTS";
        }
        if inside && !l.trim().is_empty() {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

/// Whether the module text defines operator `name` at the top level (`name ==` or `name(..) ==`).
pub fn defines(src: &str, name: &str) -> bool {
    src.lines().any(|l| {
        let Some(rest) = l.strip_prefix(name) else { return false };
        let rest = rest.trim_start();
        rest.starts_with("==")
            || (rest.starts_with('(') && rest.split_once(')').map(|(_, t)| t.trim_start().starts_with("==")).unwrap_or(false))
    })
}

/// The trace module: the log as a tuple constant and
/// a spec that consumes one record per step through the target's `TraceStep(e)`.
pub fn trace_module(spec_module: &str, trace: &[Value]) -> Result<String, FormalError> {
    let mut records = Vec::with_capacity(trace.len());
    for (i, r) in trace.iter().enumerate() {
        records
            .push(tla::to_tla(r).map_err(|e| {
                FormalError::Invalid(format!("trace record {} cannot be written as TLA+: {e}; nothing was checked", i + 1))
            })?);
    }
    let log = if records.is_empty() { "<<>>".to_string() } else { format!("<<\n    {}\n>>", records.join(",\n    ")) };
    Ok(format!(
        "---- MODULE {TRACE_MODULE} ----\n\
         EXTENDS {spec_module}, Naturals, Sequences, TLC\n\
         AhoTraceLog == {log}\n\
         VARIABLE {TRACE_INDEX}\n\
         AhoTraceVars == <<vars, {TRACE_INDEX}>>\n\
         AhoTraceInit == Init /\\ {TRACE_INDEX} = 1\n\
         AhoTraceNext ==\n    \
             \\/ /\\ {TRACE_INDEX} <= Len(AhoTraceLog)\n       \
                /\\ TraceStep(AhoTraceLog[{TRACE_INDEX}])\n       \
                /\\ {TRACE_INDEX}' = {TRACE_INDEX} + 1\n    \
             \\/ /\\ {TRACE_INDEX} > Len(AhoTraceLog)\n       \
                /\\ UNCHANGED AhoTraceVars\n\
         ====\n"
    ))
}

pub fn trace_cfg(constants: &str, trace_inv: bool) -> String {
    let mut out = constants.to_string();
    out.push_str("INIT AhoTraceInit\nNEXT AhoTraceNext\n");
    if trace_inv {
        out.push_str("INVARIANT TraceInv\n");
    }
    out.push_str("CHECK_DEADLOCK TRUE\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> CheckConfig {
        CheckConfig {
            invariants: vec!["TypeOK".into(), "AtMostOneResult".into()],
            constants: vec![("Calls".into(), "{c1, c2}".into()), ("MaxLen".into(), "4".into())],
            ..CheckConfig::default()
        }
    }

    #[test]
    fn cfg_has_constants_init_next_invariants_in_order() {
        let text = render_cfg(&CheckConfig { check_deadlock: true, properties: vec!["Live".into()], ..cfg() }, &["AhoDepthBound"]).unwrap();
        assert_eq!(
            text,
            "CONSTANTS\n    Calls = {c1, c2}\n    MaxLen = 4\nINIT Init\nNEXT Next\nCONSTRAINT AhoDepthBound\nINVARIANT TypeOK\nINVARIANT AtMostOneResult\nPROPERTY Live\nCHECK_DEADLOCK TRUE\n"
        );
        assert!(render_cfg(&CheckConfig::default(), &[]).unwrap().ends_with("CHECK_DEADLOCK FALSE\n"));
    }

    #[test]
    fn cfg_refuses_what_would_change_its_meaning() {
        let bad = |c: CheckConfig| matches!(render_cfg(&c, &[]), Err(FormalError::Invalid(_)));
        assert!(bad(CheckConfig { invariants: vec!["Inv\nNEXT Other".into()], ..cfg() }));
        assert!(bad(CheckConfig { constants: vec![("N".into(), "3\nINIT X".into())], ..cfg() }));
        assert!(bad(CheckConfig { constants: vec![("N M".into(), "3".into())], ..cfg() }));
        assert!(bad(CheckConfig { init: "".into(), ..cfg() }));
    }

    #[test]
    fn constant_sections_are_copied_verbatim() {
        let text =
            "\\* model\nCONSTANTS\n    Calls = {c1, c2}\n    Max = 3\nINIT Init\nNEXT Next\nCONSTANT Turns = {1}\nINVARIANT TypeOK\n";
        assert_eq!(constant_sections(text), "CONSTANTS\n    Calls = {c1, c2}\n    Max = 3\nCONSTANT Turns = {1}\n");
        assert_eq!(constant_sections("INIT Init\n"), "");
    }

    #[test]
    fn definitions_are_found_at_top_level_only() {
        let src = "TraceStep(e) ==\n  TRUE\nTraceInv == TRUE\n  Nested == 1\nInit==x=0\n";
        assert!(defines(src, "TraceStep") && defines(src, "TraceInv") && defines(src, "Init"));
        assert!(!defines(src, "Nested") && !defines(src, "Trace"));
    }

    #[test]
    fn trace_module_embeds_records_and_steps_through_them() {
        let m =
            trace_module("Log", &[json!({"action": "TurnStart", "id": 1, "turn": 1}), json!({"action": "Other", "id": 2, "kind": "Note"})])
                .unwrap();
        assert!(m.starts_with("---- MODULE AhoTrace ----\nEXTENDS Log, Naturals, Sequences, TLC\n"));
        assert!(m.contains("[action |-> \"TurnStart\", id |-> 1, turn |-> 1],\n    [action |-> \"Other\", id |-> 2, kind |-> \"Note\"]"));
        assert!(m.contains("/\\ TraceStep(AhoTraceLog[ahoTraceIdx])"));
        assert!(m.contains("/\\ UNCHANGED AhoTraceVars"));
        assert!(trace_module("Log", &[]).unwrap().contains("AhoTraceLog == <<>>"));
        assert!(matches!(trace_module("Log", &[json!({"x": 1.5})]), Err(FormalError::Invalid(m)) if m.contains("record 1")));
        assert_eq!(
            trace_cfg("CONSTANT N = 2\n", true),
            "CONSTANT N = 2\nINIT AhoTraceInit\nNEXT AhoTraceNext\nINVARIANT TraceInv\nCHECK_DEADLOCK TRUE\n"
        );
    }

    #[test]
    fn cfg_files_round_trip_through_check_config() {
        let text = "\\* model\nCONSTANTS\n    Calls = {c1, c2}\n    Pick <- MCPick\nINIT Init\nNEXT Next\nINVARIANTS TypeOK\n    AtMostOneResult\nCHECK_DEADLOCK FALSE\n";
        let c = tla::parse_cfg(text).unwrap();
        assert_eq!(c.constants, [("Calls".to_string(), "{c1, c2}".to_string()), ("Pick".to_string(), "<- MCPick".to_string())]);
        assert_eq!(c.invariants, ["TypeOK", "AtMostOneResult"]);
        assert!(!c.check_deadlock);
        let again = tla::parse_cfg(&render_cfg(&c, &[]).unwrap()).unwrap();
        assert_eq!(again, c);
        assert!(render_cfg(&c, &[]).unwrap().contains("    Pick <- MCPick\n"));
    }

    #[test]
    fn depth_bound_counts_steps_not_states() {
        assert!(bound_module("Log", 5).contains("AhoDepthBound == TLCGet(\"level\") <= 6"));
        assert_eq!(depth_bound(CheckMode::Bounded(0)), None);
        assert_eq!(depth_bound(CheckMode::Bounded(3)), Some(3));
    }
}
