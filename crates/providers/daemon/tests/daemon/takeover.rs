use aho_log::Takeover;
use daemon::takeover::{AttachRequest, Owner, decide};

fn owner(client_instance: &str, live: bool) -> Owner {
    Owner { client_instance: client_instance.into(), live }
}

fn req(client_instance: &str, has_local_history: bool) -> AttachRequest {
    AttachRequest { client_instance: client_instance.into(), has_local_history }
}

#[test]
fn takeover_is_allowed_for_the_same_instance_or_history_without_proof_of_another_client() {
    let cases: &[(&str, Option<Owner>, AttachRequest, bool)] = &[
        ("no owner", None, req("b", false), true),
        ("same instance, owner still attached", Some(owner("a", true)), req("a", false), true),
        ("different live owner, local history", Some(owner("a", true)), req("b", true), false),
        ("dead owner, local history", Some(owner("a", false)), req("b", true), true),
        ("different dead owner, no history", Some(owner("a", false)), req("b", false), false),
        ("different live owner, no history", Some(owner("a", true)), req("b", false), false),
        ("owner without instance id, local history", Some(owner("", true)), req("b", true), true),
        ("both without instance id, no history", Some(owner("", false)), req("", false), false),
    ];
    for (name, o, r, allowed) in cases {
        let got = decide(o.as_ref(), r);
        assert_eq!(got == Takeover::Allowed, *allowed, "{name}: {got:?}");
    }
}

#[test]
fn a_refusal_names_the_owner_and_how_to_take_over() {
    let Takeover::Refused(reason) = decide(Some(&owner("tui-1", true)), &req("web-2", true)) else {
        panic!("a live, different owner must refuse");
    };
    assert!(reason.contains("`tui-1`"), "{reason}");
    assert!(reason.contains("read-only"), "{reason}");
    assert!(reason.contains("detach the other client"), "{reason}");
}
