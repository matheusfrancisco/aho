//! The committed SDK sources (`aho/sdk/`) are exactly what the generator renders from the protocol
//! table, so drift fails CI. Regenerate with `cargo run -p aho-sdk -- gen`.

use aho_sdk::codegen;
use aho_sdk::protocol::PROTOCOL;

#[test]
fn the_committed_sdk_sources_are_what_the_protocol_table_renders() {
    let files = codegen::generate(&PROTOCOL);
    let drift = codegen::drift(&codegen::default_dir(), &files);
    assert!(drift.is_empty(), "the SDK sources are stale; run `cargo run -p aho-sdk -- gen`:\n{}", drift.join("\n"));
}
