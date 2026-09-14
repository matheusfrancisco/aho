//! Real-artifact smoke (not a mocked inference test). Run only with the minilm feature.
//! `cargo run -p embed-local --features minilm --example minilm-smoke -- DIR ORT_LIBRARY [--provision]`

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aho_embed::{Embed, similarity};
use aho_log::{CapDeltaOp, EntryKind, LogStore, MemoryLog};
use embed_local::{HashNgram, MiniLm, MiniLmArtifacts, provision_minilm};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !(args.len() == 2 || args.len() == 3 && args[2] == "--provision") {
        return Err("usage: minilm-smoke ARTIFACT_DIR ONNX_RUNTIME_LIBRARY [--provision]".into());
    }
    let directory = PathBuf::from(&args[0]);
    let library = PathBuf::from(&args[1]);
    let artifacts = if args.len() == 3 { provision_minilm(&directory).await? } else { MiniLmArtifacts::open(&directory)? };
    let log = Arc::new(MemoryLog::default());
    let model = MiniLm::with_log(artifacts.clone(), 2, library.clone(), Some(log.clone()));
    assert!(!model.status().loaded);
    assert!(model.embed(&[]).await?.is_empty());
    assert!(!model.status().loaded, "empty requests must not load native weights");

    let texts =
        ["A man is eating food.", "A person is having a meal.", "The weather forecast predicts heavy snow tomorrow in the mountains."]
            .map(str::to_string);
    let vectors = model.embed(&texts).await?;
    for vector in &vectors {
        assert_eq!(vector.dims.len(), 384);
        assert!(vector.dims.iter().all(|n| n.is_finite()));
        assert!((vector.norm() - 1.0).abs() < 1e-5);
        assert_eq!(vector.model, model.model());
    }
    let near = similarity(&vectors[0], &vectors[1]).ok_or("same-model similarity was refused")?;
    let far = similarity(&vectors[0], &vectors[2]).ok_or("same-model similarity was refused")?;
    println!("model={} paraphrase_cosine={near:.6} unrelated_cosine={far:.6}", model.model());
    assert!(near > 0.60 && near > far + 0.30, "semantic paraphrase must beat unrelated text: {near} vs {far}");
    assert_eq!(model.status().cached_vectors, 2, "vector cache stays bounded");

    let uncached = MiniLm::new(artifacts, 0, library);
    let single = uncached.embed(&texts[..1]).await?;
    assert!(similarity(&single[0], &vectors[0]).ok_or("identity mismatch")? > 0.99999, "padding cannot change the sentence vector");
    let duplicates = uncached.embed(&[texts[0].clone(), texts[0].clone()]).await?;
    assert_eq!(duplicates[0], duplicates[1]);
    let long = uncached.embed(&["hello ".repeat(254), "hello ".repeat(400)]).await?;
    assert_eq!(long[0], long[1], "truncation preserves CLS/SEP within the 256-token sentence limit");
    assert_eq!(uncached.status().cached_vectors, 0);
    let lexical = HashNgram::new(0).embed(&texts[..1]).await?;
    assert_eq!(similarity(&lexical[0], &vectors[0]), None, "never compare lexical and MiniLM vector spaces");
    uncached.unload().await;
    assert!(!uncached.status().loaded);

    println!("before_idle={}", serde_json::to_string(&model.status())?);
    let sweeper = model.spawn_idle_unload(&tokio::runtime::Handle::current(), Duration::from_millis(80));
    tokio::time::timeout(Duration::from_secs(5), async {
        while model.status().loaded {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert_eq!(model.status().cached_vectors, 0);
    assert!(model.status().audit_error.is_none());
    println!("after_idle={}", serde_json::to_string(&model.status())?);
    let reloaded = model.embed(&texts[..1]).await?;
    assert!(similarity(&reloaded[0], &vectors[0]).ok_or("reload changed identity")? > 0.99999);
    let events: Vec<_> = log
        .read_from(None)
        .into_iter()
        .filter_map(|entry| match entry.kind {
            EntryKind::CapDelta { op, reason, .. } => Some((op, reason)),
            _ => None,
        })
        .collect();
    assert_eq!(
        events,
        vec![
            (CapDeltaOp::Activated, "ModelLoaded".into()),
            (CapDeltaOp::Deactivated, "Idle".into()),
            (CapDeltaOp::Activated, "ModelLoaded".into()),
        ]
    );
    println!("lifecycle={}", serde_json::to_string(&events)?);
    drop(model);
    tokio::time::timeout(Duration::from_secs(5), sweeper).await??;
    println!(
        "PASS: real ONNX semantics, normalization, padding, truncation, identity isolation, bounded cache, idle unload, lifecycle log and reload"
    );
    Ok(())
}
