use super::*;
use aho_config::{ResolveInputs, Schema, resolve};
use std::path::Path;
use world_host::{HostFs, HostWorld};

fn tool(dir: &Path) -> ToolFs {
    let fs: Arc<dyn aho_fs::Fs> = Arc::new(HostFs::new(dir).unwrap());
    ToolFs::build(FsDeps { read: FsRead(fs.clone()), write: FsWrite(fs) }, ()).unwrap()
}

fn code(o: &ExecOutcome) -> &str {
    match o {
        ExecOutcome::Err { code, .. } => code,
        ExecOutcome::Done { .. } => "done",
        ExecOutcome::Cancelled => "cancelled",
    }
}

fn text(o: &ExecOutcome) -> String {
    match o {
        ExecOutcome::Done { content, .. } => content.iter().filter_map(|b| b.as_text()).collect::<Vec<_>>().join(""),
        ExecOutcome::Err { reason, .. } => reason.clone(),
        ExecOutcome::Cancelled => String::new(),
    }
}

fn value(o: &ExecOutcome) -> &Value {
    match o {
        ExecOutcome::Done { value, .. } => value,
        other => panic!("expected Done, got {other:?}"),
    }
}

fn meta(o: &ExecOutcome) -> &Card {
    match o {
        ExecOutcome::Done { meta: Some(m), .. } => m,
        other => panic!("expected a card, got {other:?}"),
    }
}

#[tokio::test]
async fn mutating_an_existing_file_that_was_never_read_is_fs_not_observed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello world\n").unwrap();
    let t = tool(dir.path());

    let w = t.run("write", &json!({"path": "a.txt", "content": "clobbered\n"})).await;
    assert_eq!(code(&w), "FS_NOT_OBSERVED", "{w:?}");
    let e = t.run("edit", &json!({"path": "a.txt", "old_string": "hello", "new_string": "bye"})).await;
    assert_eq!(code(&e), "FS_NOT_OBSERVED");
    let s = t.run("str_replace_editor", &json!({"command": "str_replace", "path": "a.txt", "old_str": "hello", "new_str": "bye"})).await;
    assert_eq!(code(&s), "FS_NOT_OBSERVED");
    assert!(text(&w).contains("Read it first"), "message voice says how to fix it: {}", text(&w));
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "hello world\n", "nothing was written");

    let missing = t.run("edit", &json!({"path": "nope.txt", "old_string": "a", "new_string": "b"})).await;
    assert_eq!(code(&missing), "FS_NOT_FOUND", "a missing file is reported as missing, not as unread");

    assert_eq!(code(&t.run("read", &json!({"path": "./a.txt"})).await), "done");
    let e = t.run("edit", &json!({"path": "a.txt", "old_string": "hello", "new_string": "bye"})).await;
    assert_eq!(code(&e), "done", "{e:?}");
    let e2 = t.run("edit", &json!({"path": "a.txt", "old_string": "bye", "new_string": "ciao"})).await;
    assert_eq!(code(&e2), "done", "our own write keeps the ledger current");
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "ciao world\n");
}

#[tokio::test]
async fn a_file_changed_after_it_was_read_is_fs_stale_version() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "v1\n").unwrap();
    let t = tool(dir.path());
    t.run("read", &json!({"path": "a.txt"})).await;
    std::fs::write(dir.path().join("a.txt"), "v2 from the operator's editor\n").unwrap();

    let e = t.run("edit", &json!({"path": "a.txt", "old_string": "v", "new_string": "w"})).await;
    assert_eq!(code(&e), "FS_STALE_VERSION");
    let w = t.run("write", &json!({"path": "a.txt", "content": "mine\n"})).await;
    assert_eq!(code(&w), "FS_STALE_VERSION");
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "v2 from the operator's editor\n");

    t.run("read", &json!({"path": "a.txt"})).await;
    assert_eq!(code(&t.run("write", &json!({"path": "a.txt", "content": "mine\n"})).await), "done");
}

#[tokio::test]
async fn read_numbers_lines_windows_and_reports_partial() {
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    std::fs::write(dir.path().join("f.txt"), &body).unwrap();
    let t = tool(dir.path());
    let r = t.run("read", &json!({"path": "f.txt", "offset": 3, "limit": 2})).await;
    assert_eq!(text(&r), "     3\tline 3\n     4\tline 4\n(lines 3–4 of 10; pass offset/limit for more)\n");
    let v = value(&r);
    assert_eq!((v["partial"].as_bool(), v["first"].as_u64(), v["last"].as_u64()), (Some(true), Some(3), Some(4)));
    assert_eq!(v["version"].as_str(), Some(FileVersion::of(body.as_bytes()).to_hex().as_str()));
    assert_eq!(meta(&r), &Card::Read { path: "f.txt".into(), range: Some((3, 4)) });
    let full = t.run("read", &json!({"path": "f.txt"})).await;
    assert_eq!(value(&full)["partial"], json!(false));
    assert_eq!(code(&t.run("read", &json!({"path": "../etc/passwd"})).await), "FS_OUTSIDE_WORKSPACE");
    assert_eq!(code(&t.run("read", &json!({"path": 7})).await), "TOOL_BAD_ARGS");
}

#[tokio::test]
async fn edit_and_write_return_diff_cards_with_context_hunks() {
    let dir = tempfile::tempdir().unwrap();
    let t = tool(dir.path());
    let c = t.run("write", &json!({"path": "src/new.rs", "content": "fn a() {}\nfn b() {}\n"})).await;
    assert_eq!(text(&c), "Created src/new.rs (2 lines).");
    assert_eq!(value(&c)["created"], json!(true));
    let e = t.run("edit", &json!({"path": "src/new.rs", "old_string": "fn b() {}", "new_string": "fn b() { a() }"})).await;
    assert_eq!(
        meta(&e),
        &Card::Diff { path: "src/new.rs".into(), hunks: vec!["@@ -1,2 +1,2 @@\n fn a() {}\n-fn b() {}\n+fn b() { a() }".into()] }
    );
    let dup = t.run("write", &json!({"path": "src/new.rs", "content": "x\nx\n"})).await;
    assert_eq!(code(&dup), "done");
    let nu = t.run("edit", &json!({"path": "src/new.rs", "old_string": "x", "new_string": "y"})).await;
    assert_eq!(code(&nu), "FS_NOT_UNIQUE");
    let all = t.run("edit", &json!({"path": "src/new.rs", "old_string": "x", "new_string": "y", "replace_all": true})).await;
    assert_eq!(code(&all), "done");
    assert_eq!(std::fs::read_to_string(dir.path().join("src/new.rs")).unwrap(), "y\ny\n");
}

#[tokio::test]
async fn str_replace_editor_views_creates_replaces_and_inserts() {
    let dir = tempfile::tempdir().unwrap();
    let t = tool(dir.path());
    let ed = |args: Value| {
        let t = t.clone();
        async move { t.run("str_replace_editor", &args).await }
    };
    assert_eq!(code(&ed(json!({"command": "create", "path": "n.txt", "file_text": "a\nc\n"})).await), "done");
    assert_eq!(code(&ed(json!({"command": "insert", "path": "n.txt", "insert_line": 1, "new_str": "b"})).await), "done");
    assert_eq!(code(&ed(json!({"command": "insert", "path": "n.txt", "insert_line": 0, "new_str": "top\n"})).await), "done");
    assert_eq!(std::fs::read_to_string(dir.path().join("n.txt")).unwrap(), "top\na\nb\nc\n");
    assert_eq!(code(&ed(json!({"command": "insert", "path": "n.txt", "insert_line": 99, "new_str": "x"})).await), "TOOL_BAD_ARGS");
    assert_eq!(code(&ed(json!({"command": "str_replace", "path": "n.txt", "old_str": "b\n", "new_str": "B\n"})).await), "done");
    let v = ed(json!({"command": "view", "path": "n.txt", "view_range": [2, -1]})).await;
    assert_eq!(text(&v), "     2\ta\n     3\tB\n     4\tc\n(lines 2–4 of 4; pass offset/limit for more)\n");
    let d = ed(json!({"command": "view", "path": "."})).await;
    assert_eq!(text(&d), "n.txt");
    assert_eq!(code(&ed(json!({"command": "undo_edit", "path": "n.txt"})).await), "TOOL_BAD_ARGS");

    std::fs::write(dir.path().join("other.txt"), "o\n").unwrap();
    let blind = ed(json!({"command": "insert", "path": "other.txt", "insert_line": 0, "new_str": "x"})).await;
    assert_eq!(code(&blind), "FS_NOT_OBSERVED");
    assert_eq!(editor_concurrency(&json!({"command": "view"})), Concurrency::Safe);
    assert_eq!(editor_concurrency(&json!({"command": "create"})), Concurrency::Exclusive);
}

#[tokio::test]
async fn glob_and_grep_return_sorted_hits_and_search_cards() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".gitignore"), "gen/\n").unwrap();
    std::fs::create_dir_all(dir.path().join("gen")).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("gen/x.rs"), "TODO\n").unwrap();
    std::fs::write(dir.path().join("src/b.rs"), "ok\n// TODO b\n").unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "// TODO a\n").unwrap();
    let t = tool(dir.path());
    let g = t.run("glob", &json!({"pattern": "**/*.rs"})).await;
    assert_eq!(value(&g)["files"], json!(["src/a.rs", "src/b.rs"]));
    assert_eq!(meta(&g), &Card::Search { query: "**/*.rs".into(), hits: vec!["src/a.rs".into(), "src/b.rs".into()] });
    let r = t.run("grep", &json!({"pattern": "TODO (a|b)", "include": "*.rs"})).await;
    assert_eq!(text(&r), "src/a.rs:1: // TODO a\nsrc/b.rs:2: // TODO b");
    assert_eq!(code(&t.run("grep", &json!({"pattern": "["})).await), "FS_INVALID");
    let none = t.run("glob", &json!({"pattern": "*.py"})).await;
    assert_eq!(text(&none), "No files match `*.py`.");
}

#[tokio::test]
async fn read_image_returns_an_image_block() {
    let dir = tempfile::tempdir().unwrap();
    let png: Vec<u8> = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    std::fs::write(dir.path().join("p.png"), &png).unwrap();
    std::fs::write(dir.path().join("t.txt"), "text").unwrap();
    let t = tool(dir.path());
    let r = t.run("read_image", &json!({"path": "p.png"})).await;
    match &r {
        ExecOutcome::Done { content, value, .. } => {
            assert_eq!(content[1], ContentBlock::Image { media_type: "image/png".into(), data: png.clone() });
            assert_eq!(value["bytes"], json!(png.len()));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(code(&t.run("read_image", &json!({"path": "t.txt"})).await), "FS_NOT_AN_IMAGE");
}

#[test]
fn schemas_are_closed_objects_whose_required_fields_exist() {
    let world = BoundWorld::host();
    for d in TOOLS {
        let s = (d.schema)(&world).0;
        assert_eq!(s["type"], "object", "{}", d.name);
        assert_eq!(s["additionalProperties"], json!(false), "{}", d.name);
        let props = s["properties"].as_object().unwrap();
        for r in s["required"].as_array().unwrap() {
            assert!(props.contains_key(r.as_str().unwrap()), "{}: required `{r}` has no property", d.name);
        }
    }
}

#[tokio::test]
async fn runs_through_a_composition_with_the_host_world() {
    let ws = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("hello.txt"), "hi\n").unwrap();
    let comp = Composition::new("t").provide::<HostWorld>().consume::<ToolFs>();
    let mut schema = Schema::default();
    for k in comp.config_keys() {
        schema.add(k);
    }
    let mut inputs = ResolveInputs::default();
    inputs.sets = vec![
        ("workspace.root".into(), ws.path().to_string_lossy().into_owned()),
        ("workspace.shadow_dir".into(), home.path().to_string_lossy().into_owned()),
    ];
    let l = resolve(&inputs, &schema, false).unwrap();
    let mut env = Env::empty(Os::current());
    env.binaries.insert("git".into());
    let policy =
        PolicyInput { preset: "workspace".into(), allowed: ConsequenceSet::of(&[Consequence::ReadOnly, Consequence::WorkspaceWrite]) };
    let p = plan(&comp.manifests(), &l, &env, &policy, vec![]).unwrap();
    let (reg, broken) = comp.start(&p, &l);
    assert!(broken.is_empty(), "{broken:?}");
    let (decl, exec) = reg.tool("read").unwrap();
    assert_eq!(decl.max_inline, None);
    let req = ExecRequest {
        view: ExecView {
            call: CallId("c1".into()),
            tool: "read".into(),
            class: decl.class,
            args: json!({"path": "hello.txt"}),
            policy: PolicyView {
                epoch: Epoch(0),
                allowed: policy.allowed,
                world: BoundWorld::host(),
                workspace: ws.path().to_string_lossy().into_owned(),
                home: home.path().to_string_lossy().into_owned(),
            },
            by: Principal::system("test"),
        },
        cancel: CancelToken::new(),
        progress: None,
    };
    let out = exec(req).await;
    assert_eq!(text(&out), "     1\thi\n");
}
