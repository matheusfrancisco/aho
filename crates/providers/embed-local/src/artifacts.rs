//! Explicit host-side provisioning. Inference never downloads or substitutes another model.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use aho_embed::{EmbedError, ModelId};
use sha1::Sha1;
use sha2::{Digest, Sha256};

pub const MINILM_MODEL: &str = "all-MiniLM-L6-v2";
pub const MINILM_REVISION: &str = "1110a243fdf4706b3f48f1d95db1a4f5529b4d41";
const MODEL_SHA256: &str = "6fd5d72fe4589f189f8ebc006442dbb529bb7ce38f8082112682524616046452";
const TOKENIZER_GIT_BLOB: &str = "cb202bfe2e3c98645018a6d12f182a434c9d3e02";
const MODEL_BYTES: u64 = 90_405_214;
const TOKENIZER_BYTES: u64 = 466_247;

/// Verified, self-contained official MiniLM artifacts. Other exports must not reuse this identity.
/// Paths may live anywhere the operator explicitly permits; no ambient home directory is used.
#[derive(Clone, Debug)]
pub struct MiniLmArtifacts {
    directory: PathBuf,
    model: ModelId,
}

impl MiniLmArtifacts {
    /// Verify locally supplied `model.onnx` and `tokenizer.json`, without network access.
    pub fn open(directory: &Path) -> Result<Self, EmbedError> {
        verify_file(&directory.join("model.onnx"), false)?;
        let tokenizer_sha = verify_file(&directory.join("tokenizer.json"), true)?;
        Ok(Self {
            directory: directory.to_owned(),
            model: ModelId::new(format!("{MINILM_MODEL}:onnx-sha256-{MODEL_SHA256}:tokenizer-sha256-{tokenizer_sha}:mean-mask-l2-256-v1")),
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn model(&self) -> ModelId {
        self.model.clone()
    }

    // Revalidate the exact buffers passed to the runtime: replacement after open cannot silently
    // change the vector space, and external-data ONNX exports cannot slip past the model digest.
    #[allow(clippy::disallowed_methods, reason = "authority: embed-minilm-read")]
    pub(crate) fn load(&self) -> Result<(Vec<u8>, Vec<u8>), EmbedError> {
        let read = |name: &str, limit: u64| -> Result<Vec<u8>, EmbedError> {
            let path = self.directory.join(name);
            let file = std::fs::File::open(&path).map_err(unavailable)?;
            let mut bytes = Vec::with_capacity(limit as usize);
            file.take(limit + 1).read_to_end(&mut bytes).map_err(unavailable)?;
            Ok(bytes)
        };
        let model = read("model.onnx", MODEL_BYTES)?;
        let tokenizer = read("tokenizer.json", TOKENIZER_BYTES)?;
        verify_bytes(&model, false)?;
        verify_bytes(&tokenizer, true)?;
        Ok((model, tokenizer))
    }
}

pub(crate) fn unavailable(error: impl std::fmt::Display) -> EmbedError {
    EmbedError::ModelUnavailable(error.to_string())
}

fn verify_bytes(bytes: &[u8], tokenizer: bool) -> Result<String, EmbedError> {
    let size = if tokenizer { TOKENIZER_BYTES } else { MODEL_BYTES };
    if bytes.len() as u64 != size {
        return Err(unavailable(format!("{} has {} bytes; expected {size}", artifact_name(tokenizer), bytes.len())));
    }
    let sha256 = format!("{:x}", Sha256::digest(bytes));
    let valid = if tokenizer {
        let mut hash = Sha1::new();
        hash.update(format!("blob {size}\0"));
        hash.update(bytes);
        format!("{:x}", hash.finalize()) == TOKENIZER_GIT_BLOB
    } else {
        sha256 == MODEL_SHA256
    };
    if !valid {
        return Err(unavailable(format!("{} does not match pinned MiniLM revision {MINILM_REVISION}", artifact_name(tokenizer))));
    }
    Ok(sha256)
}

fn artifact_name(tokenizer: bool) -> &'static str {
    if tokenizer { "tokenizer.json" } else { "model.onnx" }
}

#[allow(clippy::disallowed_methods, reason = "authority: embed-minilm-verify")]
fn verify_file(path: &Path, tokenizer: bool) -> Result<String, EmbedError> {
    let file = std::fs::File::open(path).map_err(|e| unavailable(format!("{}: {e}", path.display())))?;
    let size = if tokenizer { TOKENIZER_BYTES } else { MODEL_BYTES };
    if file.metadata().map_err(unavailable)?.len() != size {
        return Err(unavailable(format!("{} has the wrong size for pinned MiniLM revision {MINILM_REVISION}", path.display())));
    }
    let mut sha256 = Sha256::new();
    let mut sha1 = tokenizer.then(Sha1::new);
    if let Some(hash) = &mut sha1 {
        hash.update(format!("blob {size}\0"));
    }
    let mut reader = std::io::BufReader::new(file).take(size + 1);
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0;
    loop {
        let n = reader.read(&mut buffer).map_err(unavailable)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        sha256.update(&buffer[..n]);
        if let Some(hash) = &mut sha1 {
            hash.update(&buffer[..n]);
        }
    }
    let digest = format!("{:x}", sha256.finalize());
    let valid = match sha1 {
        Some(hash) => format!("{:x}", hash.finalize()) == TOKENIZER_GIT_BLOB,
        None => digest == MODEL_SHA256,
    };
    if total != size || !valid {
        return Err(unavailable(format!("{} failed pinned MiniLM integrity verification", path.display())));
    }
    Ok(digest)
}

/// Download the official ONNX and tokenizer once into an explicit operator-selected directory.
/// Existing files are verified, never overwritten or silently replaced. Publication is atomic;
/// concurrent provisioners can only publish identical verified artifacts. No network is touched
/// when both artifacts already exist and verify. This function is not called by `Embed::embed`.
#[allow(clippy::disallowed_methods, reason = "authority: embed-minilm-provision")]
pub async fn provision_minilm(directory: &Path) -> Result<MiniLmArtifacts, EmbedError> {
    std::fs::create_dir_all(directory).map_err(unavailable)?;
    let client = reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(unavailable)?;
    for tokenizer in [false, true] {
        let name = artifact_name(tokenizer);
        let destination = directory.join(name);
        match std::fs::metadata(&destination) {
            Ok(_) => {
                verify_file(&destination, tokenizer)?;
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(unavailable(e)),
        }
        let remote = if tokenizer { name.to_string() } else { format!("onnx/{name}") };
        let url = format!("https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/{MINILM_REVISION}/{remote}");
        let mut response = client
            .get(url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| EmbedError::Transient(format!("MiniLM download: {e}")))?;
        let limit = if tokenizer { TOKENIZER_BYTES } else { MODEL_BYTES };
        let mut pending = tempfile::NamedTempFile::new_in(directory).map_err(unavailable)?;
        let mut written = 0_u64;
        while let Some(chunk) = response.chunk().await.map_err(|e| EmbedError::Transient(e.to_string()))? {
            written = written.saturating_add(chunk.len() as u64);
            if written > limit {
                return Err(unavailable(format!("downloaded {name} exceeds its pinned size")));
            }
            pending.write_all(&chunk).map_err(unavailable)?;
        }
        pending.as_file().sync_all().map_err(unavailable)?;
        verify_file(pending.path(), tokenizer)?;
        match pending.persist_noclobber(&destination) {
            Ok(_) => {}
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                verify_file(&destination, tokenizer)?;
            }
            Err(e) => return Err(unavailable(e)),
        }
    }
    MiniLmArtifacts::open(directory)
}
