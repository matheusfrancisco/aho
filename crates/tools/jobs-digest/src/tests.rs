use std::time::{Duration, Instant};

use aho_cap::{Delivery, JobView};
use aho_log::{DetachCause, OpStatus};
use aho_types::{CallId, ContentBlock, OpId};

use crate::Digest;

const W: Duration = Duration::from_millis(1_000);

fn view(op: u64, with: &[u64], text: &str) -> JobView {
    JobView {
        op: OpId(op),
        call: CallId(format!("c{op}")),
        tool: "bash".into(),
        status: if op == 2 { OpStatus::Failed { err: "exit 1".into() } } else { OpStatus::Completed { code: 0 } },
        cause: DetachCause::Requested,
        content: vec![ContentBlock::text(text)],
        deferred: 0,
        with: with.iter().copied().map(OpId).collect(),
    }
}

fn shown(d: &Delivery) -> String {
    match d {
        Delivery::Replace(c) => c.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n"),
        other => panic!("expected a replacement, got {other:?}"),
    }
}

#[test]
fn deliveries_offered_within_the_window_are_held_to_one_instant_and_released_as_one_digest() {
    let d = Digest::new(W);
    let t0 = Instant::now();
    let until = t0 + W;
    assert_eq!(d.decide(&view(1, &[1], "one-out"), t0), Delivery::Defer { until });
    assert_eq!(d.decide(&view(2, &[2], "two-out"), t0 + Duration::from_millis(900)), Delivery::Defer { until }, "joins the open batch");

    let digest = shown(&d.decide(&view(1, &[1, 2], "one-out"), until));
    assert!(digest.starts_with("2 background jobs finished within 1000 ms"), "{digest}");
    let (one, two) = (digest.find("## Job 1 (bash) completed").unwrap(), digest.find("## Job 2 (bash) failed").unwrap());
    assert!(one < digest.find("one-out").unwrap() && two < digest.find("two-out").unwrap() && one < two, "{digest}");
    assert_eq!(shown(&d.decide(&view(2, &[1, 2], "two-out"), until)), "Its outcome is in the digest delivered with job 1.");

    // Both were delivered: the next delivery starts a fresh batch with a fresh window.
    let later = until + Duration::from_millis(5);
    assert_eq!(d.decide(&view(3, &[3], "three-out"), later), Delivery::Defer { until: later + W });
}

#[test]
fn a_batch_of_one_is_delivered_unchanged() {
    let d = Digest::new(W);
    let t0 = Instant::now();
    assert_eq!(d.decide(&view(1, &[1], "solo"), t0), Delivery::Defer { until: t0 + W });
    assert_eq!(d.decide(&view(1, &[1], "solo"), t0 + W), Delivery::Keep);
}

#[test]
fn a_delivery_offered_once_the_window_closed_starts_the_next_batch() {
    let d = Digest::new(W);
    let t0 = Instant::now();
    assert_eq!(d.decide(&view(1, &[1], "first"), t0), Delivery::Defer { until: t0 + W });
    assert_eq!(d.decide(&view(2, &[2], "second"), t0 + W), Delivery::Defer { until: t0 + W + W }, "the window is half-open");
    // Released together anyway, the first batch holds only its own member.
    assert_eq!(d.decide(&view(1, &[1, 2], "first"), t0 + W), Delivery::Keep);
    assert_eq!(d.decide(&view(2, &[1, 2], "second"), t0 + W), Delivery::Defer { until: t0 + W + W }, "still inside its own window");
}

#[test]
fn a_member_not_offered_with_its_lead_is_left_out_and_batched_again() {
    let d = Digest::new(W);
    let t0 = Instant::now();
    d.decide(&view(1, &[1], "a"), t0);
    d.decide(&view(2, &[2], "b"), t0);
    // Another capability held job 2 back: job 1 comes alone, so there is nothing to fold.
    assert_eq!(d.decide(&view(1, &[1], "a"), t0 + W), Delivery::Keep);
    let back = t0 + W + Duration::from_millis(300);
    assert_eq!(d.decide(&view(2, &[2], "b"), back), Delivery::Defer { until: back + W }, "no pointer to a digest that does not hold it");
}
