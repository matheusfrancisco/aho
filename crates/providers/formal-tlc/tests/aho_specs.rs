//! aho's own specs (`aho/formal/`) offline: every one is trace-validatable by the
//! provider's conventions, its model config parses, and the mutation generator finds work in it.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure
#![allow(clippy::disallowed_methods)] // test fixtures touch the host by design

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use formal_tlc::{model, mutate};

fn formal_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../formal")
}

fn read(name: &str) -> String {
    std::fs::read_to_string(formal_dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

#[test]
fn every_aho_spec_can_be_model_checked_and_trace_validated() {
    for m in ["Log", "Policy", "Turn", "Tasks"] {
        let src = read(&format!("{m}.tla"));
        assert_eq!(aho_formal::tla::module_name(&src).as_deref(), Some(m));
        for op in ["Init", "Next", "vars", "TraceStep", "TraceInv", "TypeOK"] {
            assert!(model::defines(&src, op), "{m}.tla defines no {op}");
        }
        let cfg = aho_formal::tla::parse_cfg(&read(&format!("{m}.cfg"))).unwrap();
        assert!(cfg.invariants.iter().any(|i| i == "TypeOK") && cfg.invariants.len() >= 2, "{m}.cfg: {:?}", cfg.invariants);
        for i in &cfg.invariants {
            assert!(model::defines(&src, i), "{m}.cfg checks {i}, which {m}.tla does not define");
        }
    }
}

#[test]
fn mutation_generator_yields_distinct_mutants_of_log_tla() {
    let src = read("Log.tla");
    let cfg = aho_formal::tla::parse_cfg(&read("Log.cfg")).unwrap();
    let ms = mutate::mutants(&src, &cfg.next);
    assert!(ms.len() >= 3, "{:?}", ms.iter().map(|m| &m.name).collect::<Vec<_>>());
    let names: HashSet<&str> = ms.iter().map(|m| m.name.as_str()).collect();
    let texts: HashSet<&str> = ms.iter().map(|m| m.source.as_str()).collect();
    assert_eq!((names.len(), texts.len()), (ms.len(), ms.len()), "names and texts are distinct");
    for op in ["drop-guard:", "negate-guard:"] {
        assert!(ms.iter().any(|m| m.name.starts_with(op)), "no {op} mutant");
    }
    for m in &ms {
        assert!(m.source != src && m.source.contains("---- MODULE Log") && m.source.trim_end().ends_with("===="), "{}", m.name);
    }
}
