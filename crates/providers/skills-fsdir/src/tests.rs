use super::*;
use aho_config::{ResolveInputs, Schema, resolve};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn skill(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\nBody of {name}.\n")
}

struct Fixture {
    _dirs: [tempfile::TempDir; 3],
    project: PathBuf,
    home: PathBuf,
    extra: PathBuf,
}

fn fixture() -> Fixture {
    let (p, h, x) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (project, home, extra) = (p.path().to_path_buf(), h.path().to_path_buf(), x.path().to_path_buf());
    write(&project.join(".aho/skills/dup/SKILL.md"), &skill("dup", "from aho"));
    write(&project.join(".aho/skills/dup/scripts/run.sh"), "echo hi\n");
    write(&project.join(".aho/skills/dup/.hidden"), "x");
    write(&project.join(".aho/skills/broken/SKILL.md"), "---\nname: broken\ndescription: d\ncontext: sideways\n---\n");
    write(&project.join(".aho/skills/README.md"), "# not a skill\n");
    write(&project.join(".aho/skills/no-skill-md/notes.txt"), "not a skill dir");
    write(&project.join(".agents/skills/dup.md"), &skill("dup", "from agents"));
    write(&project.join(".agents/skills/agents-only.md"), &skill("agents-only", "flat file"));
    write(&home.join(".aho/skills/dup/SKILL.md"), &skill("dup", "from home"));
    write(&home.join(".aho/skills/homey/SKILL.md"), "---\nname: homey\ndescription: h\nsandbox: full\n---\n");
    write(&extra.join("dup.md"), &skill("dup", "from extra"));
    write(&extra.join("extra-one.md"), &skill("extra-one", "e"));
    Fixture { _dirs: [p, h, x], project, home, extra }
}

#[test]
fn lower_rank_roots_win_duplicates_and_both_layouts_load() {
    let f = fixture();
    let s = scan(&roots(&f.project, Some(&f.home), &[f.extra.display().to_string()]));
    let names: Vec<(&str, &str, CapOrigin)> =
        s.found.iter().map(|x| (x.template.name.as_str(), x.template.description.as_str(), x.template.origin)).collect();
    assert_eq!(
        names,
        vec![
            ("dup", "from aho", CapOrigin::Workspace),
            ("agents-only", "flat file", CapOrigin::Workspace),
            ("homey", "h", CapOrigin::UserInstalled),
            ("extra-one", "e", CapOrigin::UserInstalled),
        ]
    );
    let dup = &s.found[0].template;
    assert_eq!(dup.base_dir, f.project.join(".aho/skills/dup").display().to_string());
    assert_eq!(dup.resources, vec!["scripts/run.sh"], "SKILL.md and hidden files are not resources");
    assert_eq!(s.found[1].template.base_dir, f.project.join(".agents/skills").display().to_string());
    assert_eq!(s.found[2].dropped, vec!["sandbox"]);

    let by = f.project.join(".aho/skills/dup/SKILL.md").display().to_string();
    assert_eq!(s.shadowed.len(), 3);
    assert!(s.shadowed.iter().all(|x| x.name == "dup" && x.by == by));
    assert_eq!(s.broken.len(), 1);
    assert_eq!(s.broken[0].id, "skill.broken");
    assert!(s.broken[0].error.contains("frontmatter invalid at line 4"), "{}", s.broken[0].error);
}

#[test]
fn discoverer_rows_carry_origin_and_broken_reason() {
    let f = fixture();
    let env = Env { home: f.home.display().to_string(), project: f.project.display().to_string(), ..Env::empty(Os::current()) };
    let rows = FsSkills::new().discover(&env);
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["skill.dup", "skill.agents-only", "skill.homey", "skill.broken"], "extra dirs need `with_dirs`");
    assert!(rows.iter().all(|r| r.kind == Kind::Template && r.requires == vec!["skills-fsdir"]));
    let homey = &rows[2];
    assert_eq!(homey.origin, CapOrigin::UserInstalled);
    assert!(homey.doc.contains("policy keys dropped from its frontmatter: sandbox"), "{}", homey.doc);
    let broken = &rows[3];
    assert!(broken.broken.as_deref().unwrap().contains("`context: sideways` must be inline or fork"));
    assert_eq!(broken.source_path.as_deref(), Some(f.project.join(".aho/skills/broken/SKILL.md").display().to_string().as_str()));

    let with = FsSkills::with_dirs(vec![f.extra.display().to_string()]).discover(&env);
    assert!(with.iter().any(|r| r.id == "skill.extra-one"));
}

fn start(f: &Fixture, sets: &[(&str, String)], disable: &[&str]) -> Registry {
    let mut schema = Schema::default();
    core_keys().into_iter().chain(SkillsConfig::keys()).for_each(|k| schema.add(k));
    let mut sets: Vec<(String, String)> = sets.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    sets.push(("skills.project_dir".into(), f.project.display().to_string()));
    sets.push(("skills.home_dir".into(), f.home.display().to_string()));
    let inputs = ResolveInputs { sets, disable: disable.iter().map(|d| d.to_string()).collect(), ..ResolveInputs::default() };
    let l = resolve(&inputs, &schema, false).unwrap();
    let comp = Composition::new("t").provide::<SkillsFsdir>();
    let policy = PolicyInput { preset: "workspace".into(), allowed: ConsequenceSet::ALL };
    let p = plan(&comp.manifests(), &l, &Env::empty(Os::current()), &policy, vec![]).unwrap();
    let (reg, broken) = comp.start(&p, &l);
    assert!(broken.is_empty(), "{broken:?}");
    reg
}

#[test]
fn skills_are_unlocked_until_enabled_and_switches_are_attributed() {
    let f = fixture();
    let extra = format!("[\"{}\"]", f.extra.display());
    let reg = start(
        &f,
        &[("skills.dirs", extra), ("skills.dup.enabled", "true".into()), ("skills.homey.enabled", "false".into())],
        &["skill.extra-one"],
    );
    let active = reg.service::<Arc<Vec<Template>>>(aho_skills::SERVICE).unwrap();
    assert_eq!(active.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["dup"]);
    let status = reg.service::<Arc<Vec<SkillStatus>>>(aho_skills::STATUS_SERVICE).unwrap();
    let state =
        |name: &str| status.iter().find(|s| s.name == name && !matches!(s.state, SkillState::Shadowed { .. })).unwrap().state.clone();
    assert_eq!(state("dup"), SkillState::Active);
    assert!(matches!(state("agents-only"), SkillState::Unlocked { .. }));
    assert!(matches!(state("homey"), SkillState::Disabled { why } if why.contains("--set skills.homey.enabled")));
    assert!(matches!(state("extra-one"), SkillState::Disabled { why } if why.contains("--disable skill.extra-one")));
    assert!(matches!(state("broken"), SkillState::Broken { error } if error.contains("line 4")));
    assert_eq!(status.iter().filter(|s| matches!(s.state, SkillState::Shadowed { .. })).count(), 3);
    assert_eq!(status.iter().find(|s| s.name == "homey").unwrap().dropped, vec!["sandbox"]);
}

#[test]
fn a_non_boolean_switch_is_a_config_error() {
    let f = fixture();
    let mut schema = Schema::default();
    core_keys().into_iter().chain(SkillsConfig::keys()).for_each(|k| schema.add(k));
    let inputs = ResolveInputs {
        sets: vec![("skills.dup.enabled".into(), "\"yes\"".into()), ("skills.project_dir".into(), f.project.display().to_string())],
        ..ResolveInputs::default()
    };
    let l = resolve(&inputs, &schema, false).unwrap();
    let e = SkillsConfig::from_loadout(&l).unwrap_err();
    assert!(matches!(e, ConfigError::Type { key, .. } if key == "skills.dup.enabled"));
}
